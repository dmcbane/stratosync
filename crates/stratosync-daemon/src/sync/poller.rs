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

        let remote_files = self.backend.list_recursive("/").await?;

        // Sort so directories come before their children (shorter paths first).
        // This ensures parent directories are upserted before their children,
        // so we can look up parent inodes.
        let mut sorted: Vec<&RemoteMetadata> = remote_files.iter().collect();
        sorted.sort_by_key(|m| m.path.len());

        // Cache path → inode so we can resolve parents without DB lookups
        let mut path_to_inode: HashMap<String, Inode> = HashMap::new();

        for meta in &sorted {
            let kind = if meta.is_dir { FileKind::Directory } else { FileKind::File };

            // Resolve parent inode from the entry's path.
            // e.g. "Documents/Notes/file.txt" → parent is "Documents/Notes"
            let parent = match meta.path.rfind('/') {
                Some(idx) => {
                    let parent_path = &meta.path[..idx];
                    *path_to_inode.get(parent_path).unwrap_or(&FUSE_ROOT_INODE)
                }
                None => FUSE_ROOT_INODE, // top-level entry
            };

            let inode = self.db.upsert_remote_file(
                self.mount_id,
                parent,
                &meta.name,
                &meta.path,
                kind,
                meta.size,
                meta.mtime,
                meta.etag.as_deref(),
            ).await?;

            if meta.is_dir {
                path_to_inode.insert(meta.path.clone(), inode);
            }
        }

        debug!(
            mount_id = self.mount_id,
            files = remote_files.len(),
            "poll complete"
        );
        Ok(())
    }
}
