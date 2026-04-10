/// Backend abstraction: the `Backend` trait and `RcloneBackend` implementation.
///
/// See docs/architecture/05-backend.md for design rationale.
pub(crate) mod delta;
pub(crate) mod rclone_config;

use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Result};
use async_trait::async_trait;
use tracing::{debug, warn};

use crate::types::{RemoteMetadata, RcloneLsJsonEntry, SyncError};

// ── Remote About ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct RemoteAbout {
    /// Total capacity in bytes (None if unknown).
    pub total: Option<u64>,
    /// Used bytes (None if unknown).
    pub used:  Option<u64>,
    /// Free bytes (None if unknown).
    pub free:  Option<u64>,
}

// ── Backend trait ─────────────────────────────────────────────────────────────

#[async_trait]
pub trait Backend: Send + Sync + 'static {
    // Metadata -----------------------------------------------------------

    /// Stat a single remote path. Returns `SyncError::NotFound` if absent.
    async fn stat(&self, path: &str) -> Result<RemoteMetadata, SyncError>;

    /// List the immediate children of a remote directory.
    async fn list(&self, path: &str) -> Result<Vec<RemoteMetadata>, SyncError>;

    /// Recursively list all files under a remote path.
    /// For large remotes this may be slow; callers should run it in a background task.
    async fn list_recursive(&self, path: &str) -> Result<Vec<RemoteMetadata>, SyncError>;

    // Data transfer ------------------------------------------------------

    /// Download a remote file to a local path.
    /// The local path must be a full file path (not a directory).
    async fn download(&self, remote: &str, local: &Path) -> Result<(), SyncError>;

    // Mutations ----------------------------------------------------------

    /// Upload a local file to a remote path.
    /// `if_match` is an optional ETag for optimistic concurrency;
    /// if the remote ETag doesn't match, returns `SyncError::Conflict`.
    async fn upload(
        &self,
        local:    &Path,
        remote:   &str,
        if_match: Option<&str>,
    ) -> Result<RemoteMetadata, SyncError>;

    /// Create a remote directory (and any missing parents).
    async fn mkdir(&self, path: &str) -> Result<(), SyncError>;

    /// Delete a remote file.
    async fn delete(&self, path: &str) -> Result<(), SyncError>;

    /// Remove an empty remote directory.
    async fn rmdir(&self, path: &str) -> Result<(), SyncError>;

    /// Move/rename a remote path.
    async fn rename(&self, from: &str, to: &str) -> Result<(), SyncError>;

    // Quota / info -------------------------------------------------------

    /// Query the remote's storage quota.
    async fn about(&self) -> Result<RemoteAbout, SyncError>;

    // Change detection ---------------------------------------------------

    /// Whether this backend supports efficient delta change detection
    /// (e.g. Google Drive pageToken, OneDrive deltaLink).
    fn supports_delta(&self) -> bool;

    /// Fetch changes since a token. Returns (changes, next_token).
    /// If `!supports_delta()` this always returns `Err(SyncError::Fatal(...))`.
    async fn changes_since(
        &self,
        token: &str,
    ) -> Result<(Vec<crate::types::RemoteChange>, String), SyncError>;

    /// Get the initial change token for delta polling.
    /// Only valid when `supports_delta()` returns true.
    async fn get_start_token(&self) -> Result<String, SyncError> {
        Err(SyncError::Fatal("delta not supported for this backend".into()))
    }
}

// ── Rclone error parsing ─────────────────────────────────────────────────────

/// Extract human-readable message from rclone's JSON stderr.
/// Rclone outputs lines like: {"level":"error","msg":"the actual message",...}
/// Falls back to first line of stderr (truncated to 200 chars) if not JSON.
fn parse_rclone_error(stderr: &str) -> String {
    // Try to find the last "msg" value from rclone's JSON log lines
    for line in stderr.lines().rev() {
        let trimmed = line.trim();
        if trimmed.starts_with('{') {
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(trimmed) {
                if let Some(msg) = parsed.get("msg").and_then(|v| v.as_str()) {
                    return msg.to_string();
                }
            }
        }
    }
    // Fall back: first meaningful line, truncated
    let first = stderr.lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or(stderr);
    if first.len() > 200 {
        format!("{}...", &first[..200])
    } else {
        first.to_string()
    }
}

// ── RcloneBackend ─────────────────────────────────────────────────────────────

/// Drives rclone as an external subprocess.
/// One instance per configured mount.
pub struct RcloneBackend {
    /// The rclone remote+path prefix, e.g. "gdrive:/" or "onedrive:/Documents"
    pub remote_root: String,
    /// Path to the rclone binary (resolved at construction time).
    pub rclone_bin:  std::path::PathBuf,
    /// Extra flags to pass to every rclone invocation.
    pub extra_flags: Vec<String>,
    /// Per-operation timeout.
    pub timeout:     Duration,
    /// Optional delta provider for efficient change detection.
    delta: Option<Box<dyn delta::DeltaProvider>>,
}

impl RcloneBackend {
    pub fn new(remote_root: impl Into<String>) -> Result<Self> {
        let remote_root = remote_root.into();
        let rclone_bin = which_rclone()?;
        Ok(Self {
            delta: None, // initialized asynchronously via init_delta()
            remote_root,
            rclone_bin,
            extra_flags: vec![
                "--log-level".into(), "ERROR".into(),
                "--use-json-log".into(),
            ],
            timeout: Duration::from_secs(120),
        })
    }

    /// Attempt to initialize delta (change token) support by reading the
    /// rclone config for this remote. Called once during daemon startup.
    /// On failure, logs a warning and leaves delta as None (falls back to
    /// full listing).
    pub async fn init_delta(&mut self) {
        let remote_name = rclone_config::extract_remote_name(&self.remote_root);

        let config = match rclone_config::rclone_config_show(&self.rclone_bin, remote_name).await {
            Ok(c) => c,
            Err(e) => {
                debug!("delta init: could not read rclone config for {remote_name}: {e}");
                return;
            }
        };

        let rclone_type = match config.get("type") {
            Some(t) => t.as_str(),
            None => {
                debug!("delta init: no 'type' field in rclone config for {remote_name}");
                return;
            }
        };

        let provider_type = match delta::detect_provider(rclone_type) {
            Some(p) => p,
            None => {
                debug!("delta init: provider {rclone_type} does not support delta");
                return;
            }
        };

        match provider_type {
            delta::ProviderType::GoogleDrive => {
                let token_json = match config.get("token") {
                    Some(t) => t,
                    None => {
                        warn!("delta init: no 'token' field in rclone config for {remote_name} — delta disabled");
                        return;
                    }
                };
                let oauth = match rclone_config::parse_oauth_token(token_json) {
                    Ok(t) => t,
                    Err(e) => {
                        warn!("delta init: failed to parse OAuth token for {remote_name}: {e}");
                        return;
                    }
                };
                let client_id = config.get("client_id").cloned().unwrap_or_default();
                let client_secret = config.get("client_secret").cloned().unwrap_or_default();

                // Determine root folder ID. If the remote root has a sub-path,
                // we'd need to resolve it to a folder ID. For now, use "root"
                // for the drive root.
                let root_folder_id = config.get("root_folder_id")
                    .cloned()
                    .unwrap_or_else(|| "root".to_string());

                self.delta = Some(Box::new(delta::GoogleDriveDelta::new(
                    client_id,
                    client_secret,
                    oauth,
                    root_folder_id,
                    self.rclone_bin.clone(),
                    remote_name.to_string(),
                )));
                debug!("delta init: Google Drive delta enabled for {remote_name}");
            }
            delta::ProviderType::OneDrive => {
                let token_json = match config.get("token") {
                    Some(t) => t,
                    None => {
                        warn!("delta init: no 'token' field in rclone config for {remote_name} — delta disabled");
                        return;
                    }
                };
                let oauth = match rclone_config::parse_oauth_token(token_json) {
                    Ok(t) => t,
                    Err(e) => {
                        warn!("delta init: failed to parse OAuth token for {remote_name}: {e}");
                        return;
                    }
                };
                let client_id = config.get("client_id").cloned().unwrap_or_default();
                let client_secret = config.get("client_secret").cloned().unwrap_or_default();

                // Determine the remote sub-path. For "onedrive:/Documents",
                // extract "/Documents" as the root_path.
                let root_path = self.remote_root
                    .find(':')
                    .map(|i| &self.remote_root[i + 1..])
                    .unwrap_or("/")
                    .to_string();

                // OneDrive uses Microsoft Graph API
                let drive_url = config.get("drive_id")
                    .map(|id| format!("https://graph.microsoft.com/v1.0/drives/{id}"))
                    .unwrap_or_else(|| "https://graph.microsoft.com/v1.0/me/drive".to_string());

                self.delta = Some(Box::new(delta::OneDriveDelta::new(
                    client_id,
                    client_secret,
                    oauth,
                    root_path,
                    drive_url,
                    self.rclone_bin.clone(),
                    remote_name.to_string(),
                )));
                debug!("delta init: OneDrive delta enabled for {remote_name}");
            }
        }
    }

    /// Build a fully-qualified rclone path: `{remote_root}{rel_path}`
    fn rpath(&self, rel: &str) -> String {
        // Avoid double slashes
        let root = self.remote_root.trim_end_matches('/');
        let rel  = rel.trim_start_matches('/');
        if rel.is_empty() {
            root.to_string()
        } else {
            format!("{}/{}", root, rel)
        }
    }

    /// Run an rclone command, returning stdout bytes.
    /// Kills the process on timeout. Limits output to 256MB.
    async fn run(&self, args: &[&str]) -> Result<Vec<u8>, SyncError> {
        use tokio::process::Command as TokioCommand;

        let mut cmd = TokioCommand::new(&self.rclone_bin);
        cmd.args(args);
        for f in &self.extra_flags {
            cmd.arg(f);
        }
        // Kill the process if it outlives the timeout (don't orphan it)
        cmd.kill_on_drop(true);

        debug!(args = ?args, "rclone invocation");

        let output = match tokio::time::timeout(self.timeout, cmd.output()).await {
            Ok(Ok(out)) => out,
            Ok(Err(e)) => return Err(SyncError::Fatal(format!("failed to spawn rclone: {e}"))),
            Err(_) => {
                // kill_on_drop handles cleanup
                return Err(SyncError::Network("rclone timed out".into()));
            }
        };

        // Guard against pathologically large output
        const MAX_OUTPUT: usize = 256 * 1024 * 1024;
        if output.stdout.len() > MAX_OUTPUT {
            return Err(SyncError::Fatal(format!(
                "rclone output exceeded {}MB limit", MAX_OUTPUT / (1024 * 1024),
            )));
        }

        if output.status.success() {
            return Ok(output.stdout);
        }

        let raw_stderr = String::from_utf8_lossy(&output.stderr);
        let code       = output.status.code().unwrap_or(-1);
        let msg        = parse_rclone_error(&raw_stderr);

        debug!(code, stderr = %msg, "rclone non-zero exit");

        // Map rclone exit codes (see docs/architecture/05-backend.md)
        match code {
            3 | 4 => Err(SyncError::NotFound(msg)),
            5 | 6 => Err(SyncError::Transient(msg)),
            8     => Err(SyncError::QuotaExceeded),
            _     => {
                // Inspect message for common patterns
                let lower = msg.to_lowercase();
                if lower.contains("didn't match") || lower.contains("sourcemd5") {
                    warn!("ETag conflict detected: {msg}");
                    Err(SyncError::Conflict { local: None, remote: Some(msg) })
                } else if lower.contains("invalid_grant") || lower.contains("token")
                       && (lower.contains("expired") || lower.contains("revoked"))
                {
                    Err(SyncError::PermissionDenied(msg))
                } else if lower.contains("403") || lower.contains("permission denied") {
                    Err(SyncError::PermissionDenied(msg))
                } else if lower.contains("timeout") || lower.contains("deadline")
                       || lower.contains("connection refused") || lower.contains("dns")
                {
                    Err(SyncError::Network(msg))
                } else if lower.contains("doesn't exist") || lower.contains("not found")
                       || lower.contains("404")
                {
                    Err(SyncError::NotFound(msg))
                } else if lower.contains("quota") || lower.contains("storage full") {
                    Err(SyncError::QuotaExceeded)
                } else {
                    Err(SyncError::Fatal(msg))
                }
            }
        }
    }

    /// Parse rclone lsjson output into `Vec<RemoteMetadata>`.
    fn parse_lsjson(bytes: &[u8]) -> Result<Vec<RemoteMetadata>, SyncError> {
        let entries: Vec<RcloneLsJsonEntry> = serde_json::from_slice(bytes)
            .map_err(|e| SyncError::Fatal(format!("lsjson parse error: {e}")))?;
        entries.into_iter()
            .map(|e| RemoteMetadata::try_from(e)
                .map_err(|e| SyncError::Fatal(e.to_string())))
            .collect()
    }
}

#[async_trait]
impl Backend for RcloneBackend {
    async fn stat(&self, path: &str) -> Result<RemoteMetadata, SyncError> {
        let rp    = self.rpath(path);
        let bytes = self.run(&["lsjson", "--no-modtime=false", &rp]).await?;
        let mut list = Self::parse_lsjson(&bytes)?;
        list.pop().ok_or_else(|| SyncError::NotFound(path.to_owned()))
    }

    async fn list(&self, path: &str) -> Result<Vec<RemoteMetadata>, SyncError> {
        let rp    = self.rpath(path);
        let bytes = self.run(&["lsjson", &rp]).await?;
        Self::parse_lsjson(&bytes)
    }

    async fn list_recursive(&self, path: &str) -> Result<Vec<RemoteMetadata>, SyncError> {
        let rp    = self.rpath(path);
        // --hash requests content hashes (MD5/SHA-1) for reliable change detection.
        // Google Drive returns MD5 for free; other backends may compute on the fly.
        let bytes = self.run(&["lsjson", "--recursive", "--hash", &rp]).await?;
        Self::parse_lsjson(&bytes)
    }

    async fn download(&self, remote: &str, local: &Path) -> Result<(), SyncError> {
        // rclone copy copies into a directory, so we need the parent dir
        let local_dir = local.parent()
            .ok_or_else(|| SyncError::Fatal("download target has no parent dir".into()))?;

        tokio::fs::create_dir_all(local_dir).await
            .map_err(SyncError::Io)?;

        let rp = self.rpath(remote);

        // Use rclone copyto for exact destination path
        self.run(&[
            "copyto",
            &rp,
            local.to_str().ok_or_else(|| SyncError::Fatal("non-UTF8 path".into()))?,
            "--no-traverse",
        ]).await?;

        Ok(())
    }

    async fn upload(
        &self,
        local:    &Path,
        remote:   &str,
        if_match: Option<&str>,
    ) -> Result<RemoteMetadata, SyncError> {
        let local_str = local.to_str()
            .ok_or_else(|| SyncError::Fatal("non-UTF8 local path".into()))?;
        let rp = self.rpath(remote);

        // Phase 1: if_match check — fetch remote ETag before uploading
        if let Some(expected_etag) = if_match {
            match self.stat(remote).await {
                Ok(meta) => {
                    if let Some(ref remote_etag) = meta.etag {
                        if remote_etag != expected_etag {
                            return Err(SyncError::Conflict {
                                local:  Some(expected_etag.to_owned()),
                                remote: Some(remote_etag.clone()),
                            });
                        }
                    }
                }
                Err(SyncError::NotFound(_)) => {
                    // File doesn't exist remotely yet — new file, safe to upload
                }
                Err(e) => return Err(e),
            }
        }

        // Phase 2: upload
        self.run(&[
            "copyto",
            local_str,
            &rp,
            "--checksum",
        ]).await?;

        // Phase 3: fetch resulting metadata
        self.stat(remote).await
    }

    async fn mkdir(&self, path: &str) -> Result<(), SyncError> {
        let rp = self.rpath(path);
        self.run(&["mkdir", &rp]).await?;
        Ok(())
    }

    async fn delete(&self, path: &str) -> Result<(), SyncError> {
        let rp = self.rpath(path);
        self.run(&["deletefile", &rp]).await?;
        Ok(())
    }

    async fn rmdir(&self, path: &str) -> Result<(), SyncError> {
        let rp = self.rpath(path);
        // Use purge (remove dir + contents) rather than rmdir (empty dir only).
        // During rm -rf, child file deletes run as background tasks and may not
        // have completed on the remote when rmdir is called. purge handles this.
        self.run(&["purge", &rp]).await?;
        Ok(())
    }

    async fn rename(&self, from: &str, to: &str) -> Result<(), SyncError> {
        let rf = self.rpath(from);
        let rt = self.rpath(to);
        self.run(&["moveto", &rf, &rt]).await?;
        Ok(())
    }

    async fn about(&self) -> Result<RemoteAbout, SyncError> {
        #[derive(serde::Deserialize)]
        struct AboutJson {
            total: Option<u64>,
            used:  Option<u64>,
            free:  Option<u64>,
        }

        let bytes = self.run(&["about", "--json", &self.remote_root]).await?;
        let a: AboutJson = serde_json::from_slice(&bytes)
            .map_err(|e| SyncError::Fatal(format!("about parse error: {e}")))?;

        Ok(RemoteAbout { total: a.total, used: a.used, free: a.free })
    }

    fn supports_delta(&self) -> bool {
        self.delta.is_some()
    }

    async fn changes_since(
        &self,
        token: &str,
    ) -> Result<(Vec<crate::types::RemoteChange>, String), SyncError> {
        match &self.delta {
            Some(provider) => provider.changes_since(token).await,
            None => Err(SyncError::Fatal("delta not supported for this backend".into())),
        }
    }

    async fn get_start_token(&self) -> Result<String, SyncError> {
        match &self.delta {
            Some(provider) => provider.start_token().await,
            None => Err(SyncError::Fatal("delta not supported for this backend".into())),
        }
    }
}

// ── Locate rclone binary ──────────────────────────────────────────────────────

fn which_rclone() -> Result<std::path::PathBuf> {
    // Check STRATOSYNC_RCLONE env var first, then PATH
    if let Ok(path) = std::env::var("STRATOSYNC_RCLONE") {
        let p = std::path::PathBuf::from(&path);
        if p.is_file() {
            return Ok(p);
        }
        bail!("STRATOSYNC_RCLONE={path:?} does not point to a file");
    }

    // Walk PATH
    let path_var = std::env::var("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join("rclone");
        if candidate.is_file() {
            return Ok(candidate);
        }
    }

    bail!(
        "rclone not found. Install from https://rclone.org/install/ \
         or set STRATOSYNC_RCLONE=/path/to/rclone"
    )
}

// ── Mock backend for testing ──────────────────────────────────────────────────

pub mod mock {
    use super::*;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use std::time::SystemTime;
    use crate::types::RemoteChange;

    #[derive(Default, Clone)]
    pub struct MockBackend {
        inner: Arc<Mutex<MockInner>>,
    }

    #[derive(Default)]
    struct MockInner {
        files:           HashMap<String, (RemoteMetadata, Vec<u8>)>,
        fail_paths:      std::collections::HashSet<String>,
        call_log:        Vec<String>,
        delta_enabled:   bool,
        pending_changes: Vec<RemoteChange>,
        change_token:    u64,
        /// If set, `changes_since` returns this error instead of draining.
        delta_error:     Option<SyncError>,
    }

    impl MockBackend {
        pub fn seed_file(&self, path: &str, content: &[u8]) {
            let mut inner = self.inner.lock().unwrap();
            inner.files.insert(path.to_owned(), (
                RemoteMetadata {
                    path:      path.to_owned(),
                    name:      std::path::Path::new(path)
                                   .file_name().unwrap()
                                   .to_str().unwrap().to_owned(),
                    size:      content.len() as u64,
                    mtime:     SystemTime::now(),
                    is_dir:    false,
                    etag:      Some(format!("mock-{}", content.len())),
                    checksum:  None,
                    mime_type: None,
                },
                content.to_vec(),
            ));
        }

        /// Remove a file from the mock (simulates remote deletion).
        pub fn remove_file(&self, path: &str) {
            self.inner.lock().unwrap().files.remove(path);
        }

        /// Modify a file's content (simulates remote edit — changes etag).
        pub fn modify_file(&self, path: &str, new_content: &[u8]) {
            let mut inner = self.inner.lock().unwrap();
            if let Some((meta, data)) = inner.files.get_mut(path) {
                *data = new_content.to_vec();
                meta.size = new_content.len() as u64;
                meta.etag = Some(format!("mock-{}", new_content.len()));
                meta.mtime = SystemTime::now();
            }
        }

        pub fn fail_on(&self, path: &str) {
            self.inner.lock().unwrap().fail_paths.insert(path.to_owned());
        }

        pub fn call_log(&self) -> Vec<String> {
            self.inner.lock().unwrap().call_log.clone()
        }

        /// Enable delta (change token) support on this mock.
        pub fn enable_delta(&self) {
            self.inner.lock().unwrap().delta_enabled = true;
        }

        /// Push a change that will be returned by the next `changes_since` call.
        pub fn push_change(&self, change: RemoteChange) {
            self.inner.lock().unwrap().pending_changes.push(change);
        }

        /// Make the next `changes_since` call return this error.
        pub fn set_delta_error(&self, err: SyncError) {
            self.inner.lock().unwrap().delta_error = Some(err);
        }
    }

    #[async_trait]
    impl Backend for MockBackend {
        async fn stat(&self, path: &str) -> Result<RemoteMetadata, SyncError> {
            let inner = self.inner.lock().unwrap();
            if inner.fail_paths.contains(path) {
                return Err(SyncError::Transient(format!("mock fail: {path}")));
            }
            inner.files.get(path)
                .map(|(m, _)| m.clone())
                .ok_or_else(|| SyncError::NotFound(path.to_owned()))
        }

        async fn list(&self, _path: &str) -> Result<Vec<RemoteMetadata>, SyncError> {
            let inner = self.inner.lock().unwrap();
            Ok(inner.files.values().map(|(m, _)| m.clone()).collect())
        }

        async fn list_recursive(&self, path: &str) -> Result<Vec<RemoteMetadata>, SyncError> {
            self.list(path).await
        }

        async fn download(&self, remote: &str, local: &Path) -> Result<(), SyncError> {
            let data = {
                let inner = self.inner.lock().unwrap();
                inner.files.get(remote)
                    .map(|(_, d)| d.clone())
                    .ok_or_else(|| SyncError::NotFound(remote.to_owned()))?
            };
            tokio::fs::create_dir_all(local.parent().unwrap()).await?;
            tokio::fs::write(local, &data).await?;
            Ok(())
        }

        async fn upload(
            &self,
            local:    &Path,
            remote:   &str,
            _if_match: Option<&str>,
        ) -> Result<RemoteMetadata, SyncError> {
            let data = tokio::fs::read(local).await?;
            let meta = RemoteMetadata {
                path:      remote.to_owned(),
                name:      std::path::Path::new(remote)
                               .file_name().unwrap()
                               .to_str().unwrap().to_owned(),
                size:      data.len() as u64,
                mtime:     SystemTime::now(),
                is_dir:    false,
                etag:      Some(format!("mock-{}", data.len())),
                checksum:  None,
                mime_type: None,
            };
            self.inner.lock().unwrap().files.insert(remote.to_owned(), (meta.clone(), data));
            Ok(meta)
        }

        async fn mkdir(&self, _path: &str) -> Result<(), SyncError> { Ok(()) }
        async fn rmdir(&self, _path: &str) -> Result<(), SyncError> { Ok(()) }
        async fn delete(&self, path: &str) -> Result<(), SyncError> {
            self.inner.lock().unwrap().files.remove(path);
            Ok(())
        }
        async fn rename(&self, from: &str, to: &str) -> Result<(), SyncError> {
            let mut inner = self.inner.lock().unwrap();
            if let Some(entry) = inner.files.remove(from) {
                inner.files.insert(to.to_owned(), entry);
            }
            Ok(())
        }
        async fn about(&self) -> Result<RemoteAbout, SyncError> {
            Ok(RemoteAbout { total: Some(100 << 30), used: Some(1 << 30), free: Some(99 << 30) })
        }
        fn supports_delta(&self) -> bool {
            self.inner.lock().unwrap().delta_enabled
        }

        async fn changes_since(
            &self, _token: &str,
        ) -> Result<(Vec<RemoteChange>, String), SyncError> {
            let mut inner = self.inner.lock().unwrap();
            if let Some(err) = inner.delta_error.take() {
                return Err(err);
            }
            let changes: Vec<RemoteChange> = inner.pending_changes.drain(..).collect();
            inner.change_token += 1;
            Ok((changes, format!("mock-token-{}", inner.change_token)))
        }

        async fn get_start_token(&self) -> Result<String, SyncError> {
            Ok("mock-start-0".to_string())
        }
    }
}
