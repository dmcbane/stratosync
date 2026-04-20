/// WebDAV sidecar — manages a persistent `rclone serve webdav` subprocess
/// per mount to eliminate per-operation rclone startup overhead.
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::process::{Child, Command};
use tracing::{debug, info, warn};

pub struct WebDavSidecar {
    child:    Child,
    base_url: String,
}

impl WebDavSidecar {
    /// Start a `rclone serve webdav` subprocess for the given remote.
    ///
    /// Port is allocated as `base_port + mount_id` to avoid conflicts.
    pub async fn start(
        rclone_bin: &Path,
        remote:     &str,
        mount_id:   u32,
    ) -> Result<Self> {
        let port = 19280 + mount_id as u16;
        let addr = format!("127.0.0.1:{port}");
        let base_url = format!("http://{addr}");

        info!(remote, %addr, "starting rclone WebDAV sidecar");

        let child = Command::new(rclone_bin)
            .args([
                "serve", "webdav",
                remote,
                "--addr", &addr,
                "--log-level", "ERROR",
                "--use-json-log",
            ])
            .kill_on_drop(true)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .with_context(|| format!("failed to spawn rclone serve webdav for {remote}"))?;

        let sidecar = Self { child, base_url: base_url.clone() };

        // Wait for the server to become ready (health check)
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()?;

        for attempt in 1..=20 {
            match client.request(reqwest::Method::OPTIONS, &base_url).send().await {
                Ok(resp) if resp.status().is_success() || resp.status().as_u16() == 405 => {
                    info!(%addr, attempt, "WebDAV sidecar ready");
                    return Ok(sidecar);
                }
                Ok(resp) => {
                    debug!(attempt, status = %resp.status(), "sidecar not ready yet");
                }
                Err(reason) => {
                    debug!(attempt, %reason, "sidecar not ready yet");
                }
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }

        warn!("WebDAV sidecar did not become ready after 5s — proceeding anyway");
        Ok(sidecar)
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Stop the sidecar process gracefully.
    pub async fn stop(&mut self) {
        let _ = self.child.kill().await;
    }
}

impl Drop for WebDavSidecar {
    fn drop(&mut self) {
        // kill_on_drop handles cleanup via tokio, but belt-and-suspenders
        let _ = self.child.start_kill();
    }
}
