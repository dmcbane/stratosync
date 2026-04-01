/// RemotePoller — detects remote changes and updates the local file index.
///
/// Phase 1: polling-only (lsjson diff).
/// Phase 3: adds delta API support (GDrive pageToken, OneDrive deltaLink).
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tracing::{debug, info, warn};

use stratosync_core::{
    backend::Backend, state::StateDb, types::*, RemoteMetadata,
};

pub struct RemotePoller {
    mount_id:      u32,
    db:            Arc<StateDb>,
    backend:       Arc<dyn Backend>,
    poll_interval: Duration,
}

impl RemotePoller {
    pub fn new(
        mount_id:      u32,
        db:            Arc<StateDb>,
        backend:       Arc<dyn Backend>,
        poll_interval: Duration,
    ) -> Self {
        Self { mount_id, db, backend, poll_interval }
    }

    /// Run the poll loop forever. Call from a dedicated tokio task.
    pub async fn run(self) {
        info!(mount_id = self.mount_id, interval = ?self.poll_interval, "remote poller started");
        let mut interval = tokio::time::interval(self.poll_interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            interval.tick().await;
            if let Err(e) = self.poll_once().await {
                warn!(mount_id = self.mount_id, "poll error: {e}");
            }
        }
    }

    async fn poll_once(&self) -> Result<()> {
        debug!(mount_id = self.mount_id, "polling remote");

        // Fetch the complete remote listing
        let remote_files = self.backend.list_recursive("/").await?;

        // Build a map: remote_path → RemoteMetadata
        let _remote_map: HashMap<String, &RemoteMetadata> = remote_files.iter()
            .map(|m| (m.path.clone(), m))
            .collect();

        // We process changes file by file. A full diff against DB entries is
        // the approach here; Phase 3 replaces this with delta tokens.
        //
        // For each remote file:
        //   - If not in DB → mark as Added (upsert with status=remote)
        //   - If in DB and ETags differ → Modified (set status=stale, unless dirty/uploading)
        //
        // For each DB file not in remote listing → mark as Deleted
        // (Phase 2: actually delete from DB; Phase 1: just log)

        for meta in &remote_files {
            let kind = if meta.is_dir { FileKind::Directory } else { FileKind::File };

            // Determine parent inode (simplified: walk path components)
            // In a full implementation this would resolve the parent properly.
            // For now, we use inode 1 (root) as the parent for top-level entries.
            let parent = FUSE_ROOT_INODE;

            self.db.upsert_remote_file(
                self.mount_id,
                parent,
                &meta.name,
                &meta.path,
                kind,
                meta.size,
                meta.mtime,
                meta.etag.as_deref(),
            ).await?;
        }

        debug!(
            mount_id = self.mount_id,
            files = remote_files.len(),
            "poll complete"
        );
        Ok(())
    }
}
