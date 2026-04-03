#![allow(dead_code, unused_imports)]
/// CacheManager — enforces the per-mount cache quota via LRU eviction.
///
/// See docs/architecture/07-cache.md for design.
use std::sync::Arc;

use anyhow::Result;
use tokio::time::{interval, Duration};
use tracing::{debug, error, info, warn};

use stratosync_core::{state::StateDb, types::SyncStatus};

pub struct CacheManager {
    mount_id:   u32,
    db:         Arc<StateDb>,
    quota:      u64,
    low_mark:   f64,   // evict down to this fraction of quota
    high_mark:  f64,   // start evicting at this fraction
}

impl CacheManager {
    pub fn new(mount_id: u32, db: Arc<StateDb>, quota: u64) -> Self {
        Self {
            mount_id,
            db,
            quota,
            low_mark:  0.80,
            high_mark: 0.90,
        }
    }

    pub fn with_marks(mut self, low: f64, high: f64) -> Self {
        self.low_mark  = low;
        self.high_mark = high;
        self
    }

    /// Spawn the eviction loop as a background task.
    pub fn spawn(self) {
        let mount_id = self.mount_id;
        tokio::spawn(async move {
            self.run().await;
            error!(mount_id, "cache eviction loop exited unexpectedly");
        });
    }

    async fn run(self) {
        let mut ticker = interval(Duration::from_secs(60));
        loop {
            ticker.tick().await;
            if let Err(e) = self.maybe_evict().await {
                warn!(mount_id = self.mount_id, "eviction error: {e}");
            }
        }
    }

    /// Called immediately after a successful hydration to check quota.
    pub async fn on_file_cached(&self, inode: u64, _size: u64) {
        if let Err(e) = self.db.touch_lru(inode).await {
            warn!(inode, "touch_lru failed, eviction order may be wrong: {e}");
        }
        if let Err(e) = self.maybe_evict().await {
            warn!("post-hydrate eviction error: {e}");
        }
    }

    async fn maybe_evict(&self) -> Result<()> {
        let used  = self.db.total_cache_bytes(self.mount_id).await?;
        let high  = (self.quota as f64 * self.high_mark) as u64;

        if used <= high {
            return Ok(());
        }

        let target = (self.quota as f64 * self.low_mark) as u64;
        let to_free = used.saturating_sub(target);

        debug!(
            mount_id  = self.mount_id,
            used_mb   = used / 1_048_576,
            quota_mb  = self.quota / 1_048_576,
            to_free_mb = to_free / 1_048_576,
            "starting eviction"
        );

        let candidates = self.db.lru_eviction_candidates(self.mount_id, 200).await?;
        let mut freed  = 0u64;

        for c in candidates {
            if freed >= to_free { break; }

            // Double-check status before evicting (a write may have happened
            // since the query)
            let current = self.db.get_by_inode(c.inode).await?;
            let status  = current.as_ref().map(|e| e.status);

            match status {
                Some(SyncStatus::Cached) => {
                    match tokio::fs::remove_file(&c.cache_path).await {
                        Ok(()) => {
                            self.db.set_evicted(c.inode).await?;
                            freed += c.cache_size;
                            debug!(inode = c.inode, bytes = c.cache_size, "evicted");
                        }
                        Err(e) => {
                            warn!(inode = c.inode, path = ?c.cache_path, "evict remove error: {e}");
                        }
                    }
                }
                _ => {
                    // Skip: file became dirty/uploading between query and now
                    debug!(inode = c.inode, "skip eviction, status changed");
                }
            }
        }

        info!(
            mount_id  = self.mount_id,
            freed_mb  = freed / 1_048_576,
            "eviction complete"
        );
        Ok(())
    }

    /// Pin a file — prevents LRU eviction and triggers immediate hydration.
    pub async fn pin(&self, inode: u64) -> Result<()> {
        let conn_guard = self.db.raw_conn().await;
        conn_guard.execute(
            "INSERT INTO cache_lru (inode, last_access, pinned) VALUES (?1, unixepoch(), 1)
             ON CONFLICT(inode) DO UPDATE SET pinned=1",
            rusqlite::params![inode as i64],
        )?;
        Ok(())
    }

    /// Unpin a file.
    pub async fn unpin(&self, inode: u64) -> Result<()> {
        let conn_guard = self.db.raw_conn().await;
        conn_guard.execute(
            "UPDATE cache_lru SET pinned=0 WHERE inode=?1",
            rusqlite::params![inode as i64],
        )?;
        Ok(())
    }
}
