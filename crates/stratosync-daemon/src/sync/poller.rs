/// RemotePoller — detects remote changes and updates the local file index.
///
/// Uses ETag-based diffing: fetches the full remote listing, compares against
/// the DB snapshot in memory, and only upserts entries that actually changed.
/// A poll generation counter detects remote deletions.
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

        // ── Phase 1: Fetch remote listing and DB snapshot ────────────────────
        let remote_result = self.backend.list_recursive("/").await;
        let remote_files = remote_result?;

        const MAX_POLL_ENTRIES: usize = 500_000;
        if remote_files.len() > MAX_POLL_ENTRIES {
            anyhow::bail!(
                "remote listing has {} entries (limit {}); consider selective sync",
                remote_files.len(), MAX_POLL_ENTRIES,
            );
        }

        let db_snapshot = self.db.snapshot_remote_index(self.mount_id).await?;
        let generation = self.db.get_poll_generation(self.mount_id).await? + 1;

        // Load active tombstones for filtering
        let tombstones = self.db.active_tombstones(self.mount_id).await
            .unwrap_or_default();

        // ── Phase 2: Diff remote against DB ──────────────────────────────────
        let mut to_upsert: Vec<&RemoteMetadata> = Vec::new();
        let mut unchanged_inodes: Vec<Inode> = Vec::new();
        let mut skipped_tombstoned = 0usize;
        let mut skipped_unsafe = 0usize;

        for meta in &remote_files {
            // Safety: skip path traversal and null bytes
            if meta.path.contains("..") || meta.path.contains('\0') || meta.name.contains('/') {
                skipped_unsafe += 1;
                continue;
            }

            // Skip tombstoned paths
            if tombstones.iter().any(|t| meta.path == *t || meta.path.starts_with(&format!("{t}/"))) {
                skipped_tombstoned += 1;
                continue;
            }

            match db_snapshot.get(&meta.path) {
                Some(snap) => {
                    // Entry exists in DB — check if content changed
                    if snap.etag.as_deref() == meta.etag.as_deref()
                        && snap.size == meta.size
                    {
                        // Unchanged — just bump the generation
                        unchanged_inodes.push(snap.inode);
                    } else {
                        // Changed — needs upsert
                        to_upsert.push(meta);
                    }
                }
                None => {
                    // New entry — needs upsert
                    to_upsert.push(meta);
                }
            }
        }

        // ── Phase 3: Apply changes ───────────────────────────────────────────

        // Sort upserts by path length (parent directories before children)
        to_upsert.sort_by_key(|m| m.path.len());

        // Cache path → inode for parent resolution
        // Pre-populate from DB snapshot so unchanged dirs resolve correctly
        let mut path_to_inode: HashMap<String, Inode> = db_snapshot.iter()
            .map(|(path, snap)| (path.clone(), snap.inode))
            .collect();

        for meta in &to_upsert {
            let kind = if meta.is_dir { FileKind::Directory } else { FileKind::File };

            let parent = match meta.path.rfind('/') {
                Some(idx) => {
                    let parent_path = &meta.path[..idx];
                    *path_to_inode.get(parent_path).unwrap_or(&FUSE_ROOT_INODE)
                }
                None => FUSE_ROOT_INODE,
            };

            let inode = self.db.upsert_remote_file_gen(
                self.mount_id, parent, &meta.name, &meta.path,
                kind, meta.size, meta.mtime, meta.etag.as_deref(),
                generation,
            ).await?;

            path_to_inode.insert(meta.path.clone(), inode);
        }

        // Bump generation for unchanged entries (single transaction)
        self.db.batch_mark_generation(&unchanged_inodes, generation).await?;

        // Detect and remove entries absent from remote listing
        let deleted = self.db.delete_stale_entries(self.mount_id, generation).await?;
        if !deleted.is_empty() {
            for &(inode, ref path, ref cache_path) in &deleted {
                info!(inode, path = %path, "remote deletion detected");
                // Clean up local cache file if present
                if let Some(cp) = cache_path {
                    let _ = tokio::fs::remove_file(cp).await;
                }
            }
        }

        // Save generation for next poll
        self.db.set_poll_generation(self.mount_id, generation).await?;

        // Clean up expired tombstones
        if let Ok(cleaned) = self.db.cleanup_expired_tombstones().await {
            if cleaned > 0 {
                debug!(cleaned, "expired tombstones removed");
            }
        }

        if skipped_unsafe > 0 {
            warn!(skipped_unsafe, "entries skipped due to unsafe paths");
        }

        debug!(
            mount_id    = self.mount_id,
            total       = remote_files.len(),
            changed     = to_upsert.len(),
            unchanged   = unchanged_inodes.len(),
            deleted     = deleted.len(),
            tombstoned  = skipped_tombstoned,
            "poll complete"
        );
        Ok(())
    }
}
