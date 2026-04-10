//! Parse rclone configuration to extract OAuth tokens for delta API access.
//!
//! Uses `rclone config show <remote>` to get decrypted config, which handles
//! encrypted configs and rclone's built-in credentials transparently.

use std::collections::HashMap;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::Deserialize;

/// OAuth token parsed from rclone's `token` JSON field.
#[derive(Debug, Clone)]
pub(crate) struct OAuthToken {
    pub access_token: String,
    #[allow(dead_code)]
    pub refresh_token: String,
    pub expiry: DateTime<Utc>,
    #[allow(dead_code)]
    pub token_type: String,
}

/// Deserialization target for rclone's token JSON blob.
#[derive(Deserialize)]
struct RcloneTokenJson {
    access_token: String,
    refresh_token: String,
    expiry: String,
    token_type: Option<String>,
}

/// Extract the remote name from an rclone remote root spec.
///
/// e.g. `"gdrive:/Documents"` → `"gdrive"`, `"onedrive:"` → `"onedrive"`
pub(crate) fn extract_remote_name(remote_root: &str) -> &str {
    remote_root.split(':').next().unwrap_or(remote_root)
}

/// Run `rclone config show <remote_name>` and parse the INI-style output
/// into a key-value map.
///
/// Returns an error if rclone is not found, the remote doesn't exist, or
/// the output can't be parsed.
pub(crate) async fn rclone_config_show(
    rclone_bin: &std::path::Path,
    remote_name: &str,
) -> Result<HashMap<String, String>> {
    let output = tokio::process::Command::new(rclone_bin)
        .args(["config", "show", remote_name])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .await
        .context("failed to run rclone config show")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("rclone config show failed: {}", stderr.trim());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_ini_section(&stdout)
}

/// Parse INI-style output from `rclone config show <remote>`.
///
/// The output looks like:
/// ```text
/// [remotename]
/// type = drive
/// client_id = ...
/// token = {"access_token":"...","token_type":"Bearer",...}
/// ```
///
/// We skip the section header and parse key = value lines.
pub(crate) fn parse_ini_section(text: &str) -> Result<HashMap<String, String>> {
    let mut map = HashMap::new();

    for line in text.lines() {
        let trimmed = line.trim();
        // Skip empty lines and section headers
        if trimmed.is_empty() || trimmed.starts_with('[') {
            continue;
        }
        if let Some((key, value)) = trimmed.split_once('=') {
            map.insert(
                key.trim().to_string(),
                value.trim().to_string(),
            );
        }
    }

    if map.is_empty() {
        bail!("empty or invalid rclone config output");
    }

    Ok(map)
}

/// Force rclone to refresh its OAuth token by running a lightweight command.
///
/// `rclone config show` just reads the config file — it does NOT trigger a
/// token refresh. To get a fresh token we must run an actual rclone operation
/// against the remote so rclone authenticates (and auto-refreshes if expired).
///
/// Uses `rclone about <remote>: --json` which is a cheap metadata-only call
/// (no file listing, no data transfer).
pub(crate) async fn rclone_touch_auth(
    rclone_bin: &std::path::Path,
    remote_name: &str,
) -> Result<()> {
    let remote_spec = format!("{remote_name}:");
    let output = tokio::process::Command::new(rclone_bin)
        .args(["about", &remote_spec, "--json"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .output()
        .await
        .context("failed to run rclone about for token refresh")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("rclone about failed (token refresh trigger): {}", stderr.trim());
    }

    Ok(())
}

/// Get a fresh OAuth token by triggering an rclone refresh, then reading config.
///
/// Two-step process:
/// 1. `rclone about <remote>:` — forces rclone to authenticate, refreshing
///    the token if expired and writing it back to config.
/// 2. `rclone config show <remote>` — reads the now-fresh token.
pub(crate) async fn get_fresh_oauth_token(
    rclone_bin: &std::path::Path,
    remote_name: &str,
) -> Result<OAuthToken> {
    rclone_touch_auth(rclone_bin, remote_name).await?;
    let config = rclone_config_show(rclone_bin, remote_name).await?;
    let token_json = config.get("token")
        .ok_or_else(|| anyhow::anyhow!("no token in rclone config after refresh"))?;
    parse_oauth_token(token_json)
}

/// Parse the OAuth token from rclone's `token` JSON field value.
pub(crate) fn parse_oauth_token(json_str: &str) -> Result<OAuthToken> {
    let raw: RcloneTokenJson = serde_json::from_str(json_str)
        .context("failed to parse rclone OAuth token JSON")?;

    let expiry = DateTime::parse_from_rfc3339(&raw.expiry)
        .context("failed to parse token expiry timestamp")?
        .with_timezone(&Utc);

    Ok(OAuthToken {
        access_token: raw.access_token,
        refresh_token: raw.refresh_token,
        expiry,
        token_type: raw.token_type.unwrap_or_else(|| "Bearer".to_string()),
    })
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_remote_name_with_path() {
        assert_eq!(extract_remote_name("gdrive:/Documents"), "gdrive");
    }

    #[test]
    fn test_extract_remote_name_bare() {
        assert_eq!(extract_remote_name("onedrive:"), "onedrive");
    }

    #[test]
    fn test_extract_remote_name_no_colon() {
        assert_eq!(extract_remote_name("myremote"), "myremote");
    }

    #[test]
    fn test_parse_ini_section_typical() {
        let input = r#"[gdrive]
type = drive
client_id = 12345.apps.googleusercontent.com
client_secret = secret123
scope = drive
token = {"access_token":"ya29.abc","token_type":"Bearer","refresh_token":"1//xyz","expiry":"2026-04-10T12:00:00Z"}
team_drive =
"#;
        let map = parse_ini_section(input).unwrap();
        assert_eq!(map.get("type").unwrap(), "drive");
        assert_eq!(map.get("client_id").unwrap(), "12345.apps.googleusercontent.com");
        assert_eq!(map.get("client_secret").unwrap(), "secret123");
        assert!(map.get("token").unwrap().starts_with('{'));
    }

    #[test]
    fn test_parse_ini_section_empty() {
        let result = parse_ini_section("");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_ini_section_header_only() {
        let result = parse_ini_section("[gdrive]\n");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_oauth_token_valid() {
        let json = r#"{"access_token":"ya29.abc","token_type":"Bearer","refresh_token":"1//xyz","expiry":"2026-04-10T12:00:00Z"}"#;
        let token = parse_oauth_token(json).unwrap();
        assert_eq!(token.access_token, "ya29.abc");
        assert_eq!(token.refresh_token, "1//xyz");
        assert_eq!(token.token_type, "Bearer");
        assert_eq!(token.expiry.to_rfc3339(), "2026-04-10T12:00:00+00:00");
    }

    #[test]
    fn test_parse_oauth_token_missing_token_type() {
        let json = r#"{"access_token":"ya29.abc","refresh_token":"1//xyz","expiry":"2026-04-10T12:00:00Z"}"#;
        let token = parse_oauth_token(json).unwrap();
        assert_eq!(token.token_type, "Bearer"); // defaults
    }

    #[test]
    fn test_parse_oauth_token_malformed() {
        let result = parse_oauth_token("not json at all");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_oauth_token_missing_fields() {
        let json = r#"{"access_token":"ya29.abc"}"#;
        let result = parse_oauth_token(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_oauth_token_bad_expiry() {
        let json = r#"{"access_token":"ya29.abc","refresh_token":"1//xyz","expiry":"not-a-date"}"#;
        let result = parse_oauth_token(json);
        assert!(result.is_err());
    }
}
