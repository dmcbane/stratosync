//! Delta (change token) providers for cloud backends.
//!
//! Each cloud provider that supports incremental change feeds gets a struct
//! implementing `DeltaProvider`. The `RcloneBackend` delegates `changes_since()`
//! to the appropriate provider when available.

use std::collections::HashMap;
use std::time::SystemTime;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer};
use tokio::sync::Mutex;
use tracing::{debug, warn};

use crate::types::{RemoteChange, RemoteMetadata, SyncError};
use super::rclone_config::OAuthToken;

// ── DeltaProvider trait ──────────────────────────────────────────────────────

/// Provider-agnostic delta change feed.
#[async_trait]
pub(crate) trait DeltaProvider: Send + Sync {
    /// Get the initial start token (for fresh mounts with no prior token).
    async fn start_token(&self) -> Result<String, SyncError>;

    /// Fetch all changes since the given token.
    /// Returns (changes, next_token). Paginates internally.
    async fn changes_since(&self, token: &str) -> Result<(Vec<RemoteChange>, String), SyncError>;
}

// ── Google Drive delta provider ──────────────────────────────────────────────

const GDRIVE_CHANGES_URL: &str = "https://www.googleapis.com/drive/v3/changes";
const GDRIVE_START_TOKEN_URL: &str = "https://www.googleapis.com/drive/v3/changes/startPageToken";
const GDRIVE_FILES_URL: &str = "https://www.googleapis.com/drive/v3/files";
const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

/// Fields to request from the Changes API. Minimized to reduce response size.
const GDRIVE_CHANGES_FIELDS: &str = "\
    nextPageToken,newStartPageToken,\
    changes(removed,fileId,\
    file(id,name,mimeType,size,modifiedTime,md5Checksum,trashed,parents))";

pub(crate) struct GoogleDriveDelta {
    client: reqwest::Client,
    /// The folder ID that corresponds to the rclone remote root.
    /// "root" for the drive root, or a specific folder ID for sub-paths.
    root_folder_id: String,
    /// OAuth state, refreshed as needed.
    auth: Mutex<GoogleAuth>,
}

struct GoogleAuth {
    client_id: String,
    client_secret: String,
    refresh_token: String,
    access_token: String,
    expiry: DateTime<Utc>,
}

impl GoogleDriveDelta {
    pub(crate) fn new(
        client_id: String,
        client_secret: String,
        oauth_token: OAuthToken,
        root_folder_id: String,
    ) -> Self {
        Self {
            client: reqwest::Client::new(),
            root_folder_id,
            auth: Mutex::new(GoogleAuth {
                client_id,
                client_secret,
                refresh_token: oauth_token.refresh_token,
                access_token: oauth_token.access_token,
                expiry: oauth_token.expiry,
            }),
        }
    }

    /// Ensure we have a valid access token, refreshing if needed.
    async fn ensure_valid_token(&self) -> Result<String, SyncError> {
        let mut auth = self.auth.lock().await;
        let now = Utc::now();

        // Refresh if within 60 seconds of expiry
        if auth.expiry <= now + chrono::Duration::seconds(60) {
            debug!("refreshing Google OAuth token");
            let resp = self.client
                .post(GOOGLE_TOKEN_URL)
                .form(&[
                    ("client_id", auth.client_id.as_str()),
                    ("client_secret", auth.client_secret.as_str()),
                    ("refresh_token", auth.refresh_token.as_str()),
                    ("grant_type", "refresh_token"),
                ])
                .send()
                .await
                .map_err(|e| SyncError::Network(format!("token refresh request failed: {e}")))?;

            if resp.status() == reqwest::StatusCode::UNAUTHORIZED
                || resp.status() == reqwest::StatusCode::FORBIDDEN
            {
                return Err(SyncError::PermissionDenied(
                    "OAuth token refresh failed — credentials revoked or expired".into(),
                ));
            }
            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                return Err(SyncError::Transient(
                    format!("token refresh HTTP {status}: {body}"),
                ));
            }

            let token_resp: GoogleTokenResponse = resp.json().await
                .map_err(|e| SyncError::Fatal(format!("parse token response: {e}")))?;

            auth.access_token = token_resp.access_token;
            auth.expiry = now + chrono::Duration::seconds(token_resp.expires_in);
            debug!("Google OAuth token refreshed, expires in {}s", token_resp.expires_in);
        }

        Ok(auth.access_token.clone())
    }

    /// Map an HTTP response status to a SyncError, if it's an error.
    fn map_http_error(status: reqwest::StatusCode, body: &str) -> SyncError {
        match status.as_u16() {
            410 => SyncError::TokenExpired,
            401 | 403 => SyncError::PermissionDenied(format!("HTTP {status}: {body}")),
            429 => SyncError::Transient(format!("rate limited: {body}")),
            500..=599 => SyncError::Transient(format!("server error HTTP {status}: {body}")),
            _ => SyncError::Fatal(format!("HTTP {status}: {body}")),
        }
    }

    /// Resolve the full path for a file by walking up its parent chain.
    /// Returns None if the file is not under our root_folder_id.
    fn resolve_path(
        &self,
        file_id: &str,
        file_name: &str,
        parents: &[String],
        id_map: &HashMap<String, (String, Vec<String>)>, // id → (name, parents)
    ) -> Option<String> {
        if parents.is_empty() {
            return None; // orphaned file
        }

        let parent_id = &parents[0]; // GDrive files have exactly one parent

        // Walk up the parent chain to build path components
        let mut components = vec![file_name.to_string()];
        let mut current_parent = parent_id.clone();

        // Safety: limit depth to prevent infinite loops
        for _ in 0..100 {
            if current_parent == self.root_folder_id {
                // Reached our root — reverse and join
                components.reverse();
                return Some(components.join("/"));
            }

            if let Some((name, grandparents)) = id_map.get(&current_parent) {
                components.push(name.clone());
                if grandparents.is_empty() {
                    return None; // orphaned parent chain
                }
                current_parent = grandparents[0].clone();
            } else {
                // Parent not in our change set and not root — we'd need an
                // API call to resolve. For now, skip this file.
                debug!(
                    file_id = file_id,
                    parent_id = %current_parent,
                    "skipping file: parent not in change set or cache"
                );
                return None;
            }
        }

        warn!(file_id = file_id, "path resolution exceeded depth limit");
        None
    }

    /// Fetch metadata for a single file by ID (used for parent resolution).
    async fn get_file_metadata(
        &self,
        file_id: &str,
        token: &str,
    ) -> Result<Option<(String, Vec<String>)>, SyncError> {
        let resp = self.client
            .get(&format!("{GDRIVE_FILES_URL}/{file_id}"))
            .bearer_auth(token)
            .query(&[("fields", "name,parents")])
            .send()
            .await
            .map_err(|e| SyncError::Network(format!("get file metadata: {e}")))?;

        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(Self::map_http_error(status, &body));
        }

        let file: GDriveFileMetadata = resp.json().await
            .map_err(|e| SyncError::Fatal(format!("parse file metadata: {e}")))?;

        Ok(Some((file.name, file.parents.unwrap_or_default())))
    }
}

#[async_trait]
impl DeltaProvider for GoogleDriveDelta {
    async fn start_token(&self) -> Result<String, SyncError> {
        let token = self.ensure_valid_token().await?;

        let resp = self.client
            .get(GDRIVE_START_TOKEN_URL)
            .bearer_auth(&token)
            .send()
            .await
            .map_err(|e| SyncError::Network(format!("get start token: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(Self::map_http_error(status, &body));
        }

        let body: GDriveStartTokenResponse = resp.json().await
            .map_err(|e| SyncError::Fatal(format!("parse start token: {e}")))?;

        Ok(body.start_page_token)
    }

    async fn changes_since(
        &self,
        page_token: &str,
    ) -> Result<(Vec<RemoteChange>, String), SyncError> {
        let access_token = self.ensure_valid_token().await?;
        let mut changes = Vec::new();
        let mut current_token = page_token.to_string();

        // id → (name, parents) for path resolution
        let mut id_map: HashMap<String, (String, Vec<String>)> = HashMap::new();

        // Phase 1: Paginate through all changes and collect raw data
        let mut raw_changes: Vec<GDriveChange> = Vec::new();

        let next_token = loop {
            let resp = self.client
                .get(GDRIVE_CHANGES_URL)
                .bearer_auth(&access_token)
                .query(&[
                    ("pageToken", current_token.as_str()),
                    ("pageSize", "1000"),
                    ("fields", GDRIVE_CHANGES_FIELDS),
                    ("includeRemoved", "true"),
                    ("restrictToMyDrive", "true"),
                ])
                .send()
                .await
                .map_err(|e| SyncError::Network(format!("changes request: {e}")))?;

            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                return Err(Self::map_http_error(status, &body));
            }

            let page: GDriveChangesResponse = resp.json().await
                .map_err(|e| SyncError::Fatal(format!("parse changes: {e}")))?;

            for change in &page.changes {
                if let Some(ref file) = change.file {
                    id_map.insert(
                        file.id.clone(),
                        (file.name.clone(), file.parents.clone().unwrap_or_default()),
                    );
                }
            }

            raw_changes.extend(page.changes);

            if let Some(nst) = page.new_start_page_token {
                break nst;
            } else if let Some(npt) = page.next_page_token {
                current_token = npt;
            } else {
                return Err(SyncError::Fatal(
                    "changes response has neither nextPageToken nor newStartPageToken".into(),
                ));
            }
        };

        // Phase 2: Resolve missing parents from the API
        // Collect parent IDs we need but don't have
        let mut missing_parents: Vec<String> = Vec::new();
        for change in &raw_changes {
            if let Some(ref file) = change.file {
                if let Some(ref parents) = file.parents {
                    for pid in parents {
                        if *pid != self.root_folder_id && !id_map.contains_key(pid) {
                            missing_parents.push(pid.clone());
                        }
                    }
                }
            }
        }
        missing_parents.dedup();

        // Fetch missing parents (limited to avoid excessive API calls)
        let max_parent_lookups = 50;
        for pid in missing_parents.iter().take(max_parent_lookups) {
            if let Ok(Some((name, parents))) =
                self.get_file_metadata(pid, &access_token).await
            {
                id_map.insert(pid.clone(), (name, parents));
            }
        }
        if missing_parents.len() > max_parent_lookups {
            warn!(
                missing = missing_parents.len(),
                fetched = max_parent_lookups,
                "too many unknown parents; some files may be skipped"
            );
        }

        // Phase 3: Convert raw changes to RemoteChange
        for change in raw_changes {
            if change.removed {
                // File removed — we don't have the path easily from a removal.
                // Store the file_id; the poller can look up the path from the DB.
                // For now, we try to resolve the path from id_map if available.
                if let Some((name, parents)) = id_map.get(&change.file_id) {
                    if let Some(path) = self.resolve_path(&change.file_id, name, parents, &id_map) {
                        changes.push(RemoteChange::Deleted { path });
                    }
                }
                continue;
            }

            let file = match change.file {
                Some(f) => f,
                None => continue,
            };

            if file.trashed {
                // Trashed = deleted
                let parents = file.parents.as_deref().unwrap_or(&[]);
                if let Some(path) = self.resolve_path(&file.id, &file.name, parents, &id_map) {
                    changes.push(RemoteChange::Deleted { path });
                }
                continue;
            }

            let parents = file.parents.as_deref().unwrap_or(&[]);
            let path = match self.resolve_path(&file.id, &file.name, parents, &id_map) {
                Some(p) => p,
                None => continue, // not under our root
            };

            let is_dir = file.mime_type.as_deref()
                == Some("application/vnd.google-apps.folder");

            let mtime = file.modified_time.as_deref()
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|dt| SystemTime::from(dt))
                .unwrap_or(SystemTime::UNIX_EPOCH);

            let meta = RemoteMetadata {
                path,
                name: file.name.clone(),
                size: file.size.unwrap_or(0) as u64,
                mtime,
                is_dir,
                etag: file.md5_checksum.clone(),
                checksum: None,
                mime_type: file.mime_type.clone(),
            };

            changes.push(RemoteChange::Added { meta });
        }

        debug!(
            change_count = changes.len(),
            "Google Drive delta fetch complete"
        );

        Ok((changes, next_token))
    }
}

// ── Google Drive API response types ──────────────────────────────────────────

#[derive(Deserialize)]
struct GoogleTokenResponse {
    access_token: String,
    expires_in: i64,
}

#[derive(Deserialize)]
struct GDriveStartTokenResponse {
    #[serde(rename = "startPageToken")]
    start_page_token: String,
}

#[derive(Deserialize)]
struct GDriveChangesResponse {
    #[serde(default)]
    changes: Vec<GDriveChange>,
    #[serde(rename = "nextPageToken")]
    next_page_token: Option<String>,
    #[serde(rename = "newStartPageToken")]
    new_start_page_token: Option<String>,
}

#[derive(Deserialize)]
struct GDriveChange {
    #[serde(default)]
    removed: bool,
    #[serde(rename = "fileId")]
    file_id: String,
    file: Option<GDriveFileMetadata>,
}

#[derive(Deserialize)]
struct GDriveFileMetadata {
    id: String,
    name: String,
    #[serde(rename = "mimeType")]
    mime_type: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_string_i64")]
    size: Option<i64>,
    #[serde(rename = "modifiedTime")]
    modified_time: Option<String>,
    #[serde(rename = "md5Checksum")]
    md5_checksum: Option<String>,
    #[serde(default)]
    trashed: bool,
    parents: Option<Vec<String>>,
}

// ── OneDrive delta provider (stub) ───────────────────────────────────────────

pub(crate) struct OneDriveDelta {
    _private: (), // prevent construction outside this module
}

impl OneDriveDelta {
    #[allow(dead_code)]
    pub(crate) fn new() -> Self {
        Self { _private: () }
    }
}

#[async_trait]
impl DeltaProvider for OneDriveDelta {
    async fn start_token(&self) -> Result<String, SyncError> {
        Err(SyncError::Fatal("OneDrive delta not yet implemented".into()))
    }

    async fn changes_since(
        &self,
        _token: &str,
    ) -> Result<(Vec<RemoteChange>, String), SyncError> {
        Err(SyncError::Fatal("OneDrive delta not yet implemented".into()))
    }
}

// ── Detect provider type from rclone remote name ─────────────────────────────

/// Which delta provider to use, based on the rclone remote type.
pub(crate) enum ProviderType {
    GoogleDrive,
    OneDrive,
}

/// Detect the provider type from the rclone remote's `type` field.
pub(crate) fn detect_provider(rclone_type: &str) -> Option<ProviderType> {
    match rclone_type {
        "drive" => Some(ProviderType::GoogleDrive),
        "onedrive" => Some(ProviderType::OneDrive),
        _ => None,
    }
}

/// Google Drive returns `size` as a string (e.g., `"1048576"`), not a number.
fn deserialize_optional_string_i64<'de, D>(deserializer: D) -> Result<Option<i64>, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de;

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StringOrInt {
        Str(String),
        Int(i64),
    }

    let opt: Option<StringOrInt> = Option::deserialize(deserializer)?;
    match opt {
        None => Ok(None),
        Some(StringOrInt::Int(n)) => Ok(Some(n)),
        Some(StringOrInt::Str(s)) => s.parse::<i64>()
            .map(Some)
            .map_err(|_| de::Error::custom(format!("invalid size string: {s}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── detect_provider tests ────────────────────────────────────────────

    #[test]
    fn test_detect_gdrive() {
        assert!(matches!(detect_provider("drive"), Some(ProviderType::GoogleDrive)));
    }

    #[test]
    fn test_detect_onedrive() {
        assert!(matches!(detect_provider("onedrive"), Some(ProviderType::OneDrive)));
    }

    #[test]
    fn test_detect_unknown() {
        assert!(detect_provider("s3").is_none());
        assert!(detect_provider("sftp").is_none());
    }

    // ── Google Drive JSON parsing tests ──────────────────────────────────

    #[test]
    fn test_parse_changes_response() {
        let json = r#"{
            "changes": [
                {
                    "removed": false,
                    "fileId": "abc123",
                    "file": {
                        "id": "abc123",
                        "name": "report.pdf",
                        "mimeType": "application/pdf",
                        "size": "1048576",
                        "modifiedTime": "2026-04-10T12:00:00Z",
                        "md5Checksum": "d41d8cd98f00b204e9800998ecf8427e",
                        "trashed": false,
                        "parents": ["root"]
                    }
                }
            ],
            "newStartPageToken": "token-999"
        }"#;
        let resp: GDriveChangesResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.changes.len(), 1);
        assert_eq!(resp.new_start_page_token.unwrap(), "token-999");
        assert!(!resp.changes[0].removed);
        let file = resp.changes[0].file.as_ref().unwrap();
        assert_eq!(file.name, "report.pdf");
        assert_eq!(file.size.unwrap(), 1048576);
    }

    #[test]
    fn test_parse_removed_change() {
        let json = r#"{
            "changes": [{"removed": true, "fileId": "del456"}],
            "newStartPageToken": "token-100"
        }"#;
        let resp: GDriveChangesResponse = serde_json::from_str(json).unwrap();
        assert!(resp.changes[0].removed);
        assert!(resp.changes[0].file.is_none());
    }

    #[test]
    fn test_parse_trashed_file() {
        let json = r#"{
            "changes": [{
                "removed": false,
                "fileId": "trash789",
                "file": {
                    "id": "trash789",
                    "name": "old.txt",
                    "trashed": true,
                    "parents": ["root"]
                }
            }],
            "newStartPageToken": "token-101"
        }"#;
        let resp: GDriveChangesResponse = serde_json::from_str(json).unwrap();
        assert!(resp.changes[0].file.as_ref().unwrap().trashed);
    }

    #[test]
    fn test_parse_paginated_response() {
        let json = r#"{
            "changes": [],
            "nextPageToken": "page2-token"
        }"#;
        let resp: GDriveChangesResponse = serde_json::from_str(json).unwrap();
        assert!(resp.new_start_page_token.is_none());
        assert_eq!(resp.next_page_token.unwrap(), "page2-token");
    }

    #[test]
    fn test_parse_folder_mime_type() {
        let json = r#"{
            "id": "folder1",
            "name": "Documents",
            "mimeType": "application/vnd.google-apps.folder",
            "trashed": false,
            "parents": ["root"]
        }"#;
        let file: GDriveFileMetadata = serde_json::from_str(json).unwrap();
        assert_eq!(file.mime_type.as_deref(), Some("application/vnd.google-apps.folder"));
        assert!(file.size.is_none());
    }

    #[test]
    fn test_parse_start_token_response() {
        let json = r#"{"startPageToken": "12345"}"#;
        let resp: GDriveStartTokenResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.start_page_token, "12345");
    }

    #[test]
    fn test_parse_token_response() {
        let json = r#"{"access_token": "ya29.new", "expires_in": 3600}"#;
        let resp: GoogleTokenResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.access_token, "ya29.new");
        assert_eq!(resp.expires_in, 3600);
    }

    // ── Path resolution tests ────────────────────────────────────────────

    #[test]
    fn test_resolve_path_direct_child_of_root() {
        let delta = GoogleDriveDelta::new(
            String::new(), String::new(),
            OAuthToken {
                access_token: String::new(),
                refresh_token: String::new(),
                expiry: Utc::now(),
                token_type: "Bearer".into(),
            },
            "root".into(),
        );

        let id_map = HashMap::new();
        let path = delta.resolve_path("file1", "report.pdf", &["root".into()], &id_map);
        assert_eq!(path.unwrap(), "report.pdf");
    }

    #[test]
    fn test_resolve_path_nested() {
        let delta = GoogleDriveDelta::new(
            String::new(), String::new(),
            OAuthToken {
                access_token: String::new(),
                refresh_token: String::new(),
                expiry: Utc::now(),
                token_type: "Bearer".into(),
            },
            "root".into(),
        );

        let mut id_map = HashMap::new();
        id_map.insert("folder_a".into(), ("Documents".into(), vec!["root".into()]));
        id_map.insert("folder_b".into(), ("Work".into(), vec!["folder_a".into()]));

        let path = delta.resolve_path("file1", "report.pdf", &["folder_b".into()], &id_map);
        assert_eq!(path.unwrap(), "Documents/Work/report.pdf");
    }

    #[test]
    fn test_resolve_path_not_under_root() {
        let delta = GoogleDriveDelta::new(
            String::new(), String::new(),
            OAuthToken {
                access_token: String::new(),
                refresh_token: String::new(),
                expiry: Utc::now(),
                token_type: "Bearer".into(),
            },
            "specific_folder_id".into(),
        );

        let mut id_map = HashMap::new();
        // Parent chain doesn't reach our root folder
        id_map.insert("other_root".into(), ("Other".into(), vec![]));

        let path = delta.resolve_path("file1", "report.pdf", &["other_root".into()], &id_map);
        assert!(path.is_none());
    }

    #[test]
    fn test_resolve_path_no_parents() {
        let delta = GoogleDriveDelta::new(
            String::new(), String::new(),
            OAuthToken {
                access_token: String::new(),
                refresh_token: String::new(),
                expiry: Utc::now(),
                token_type: "Bearer".into(),
            },
            "root".into(),
        );

        let id_map = HashMap::new();
        let path = delta.resolve_path("file1", "orphan.txt", &[], &id_map);
        assert!(path.is_none());
    }

    // ── HTTP error mapping tests ─────────────────────────────────────────

    #[test]
    fn test_map_410_to_token_expired() {
        let err = GoogleDriveDelta::map_http_error(
            reqwest::StatusCode::GONE, "sync token expired",
        );
        assert!(matches!(err, SyncError::TokenExpired));
    }

    #[test]
    fn test_map_401_to_permission_denied() {
        let err = GoogleDriveDelta::map_http_error(
            reqwest::StatusCode::UNAUTHORIZED, "invalid credentials",
        );
        assert!(matches!(err, SyncError::PermissionDenied(_)));
    }

    #[test]
    fn test_map_429_to_transient() {
        let err = GoogleDriveDelta::map_http_error(
            reqwest::StatusCode::TOO_MANY_REQUESTS, "rate limited",
        );
        assert!(matches!(err, SyncError::Transient(_)));
    }

    #[test]
    fn test_map_500_to_transient() {
        let err = GoogleDriveDelta::map_http_error(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR, "oops",
        );
        assert!(matches!(err, SyncError::Transient(_)));
    }
}
