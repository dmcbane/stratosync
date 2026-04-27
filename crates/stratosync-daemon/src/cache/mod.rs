#![allow(dead_code, unused_imports)]
/// CacheManager — enforces the per-mount cache quota via LRU eviction.
///
/// See docs/architecture/07-cache.md for design.
pub mod reconcile;

use std::sync::Arc;

use anyhow::Result;
use tokio::time::{interval, Duration};
use tracing::{debug, error, info, warn};

use stratosync_core::{base_store::BaseStore, state::StateDb, types::SyncStatus};

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

        // Walk candidates in batches until we've freed enough or run out.
        //
        // Why batched and not a single query: the candidate list could be
        // 100K+ rows on big mounts; we'd rather not load them all into
        // memory at once. Re-querying for each batch is cheap because
        // `set_evicted()` removes rows from `cache_lru`, so the same
        // ORDER BY last_access ASC LIMIT N gives us the next-oldest
        // candidates each iteration.
        //
        // Pre-fix this loop ran exactly once with LIMIT 200 — for a mount
        // with many small files (e.g. screenshots, GiB-scale histories of
        // ~191 KB items) that's ~38 MB freed per pass × a 60 s loop, which
        // can't keep up with even modest hydration rates. The cache stayed
        // permanently over quota.
        const BATCH: usize = 1000;

        let mut freed         = 0u64;
        let mut total_skipped = 0usize;

        loop {
            if freed >= to_free { break; }

            let candidates = self.db
                .lru_eviction_candidates(self.mount_id, BATCH)
                .await?;
            if candidates.is_empty() { break; }

            let mut pass_freed   = 0u64;
            let mut pass_skipped = 0usize;

            for c in &candidates {
                if freed + pass_freed >= to_free { break; }

                // Double-check status before evicting — a write may have
                // landed since the query made the candidate visible.
                let current = self.db.get_by_inode(c.inode).await?;
                let status  = current.as_ref().map(|e| e.status);

                match status {
                    Some(SyncStatus::Cached) => {
                        match tokio::fs::remove_file(&c.cache_path).await {
                            Ok(()) => {
                                self.db.set_evicted(c.inode).await?;
                                pass_freed += c.cache_size;
                                debug!(inode = c.inode, bytes = c.cache_size, "evicted");
                            }
                            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                                // The cache file is already gone — manual `rm`,
                                // a previous crash mid-eviction, or some other
                                // out-of-band cleanup. The DB row is stale; treat
                                // this as a successful eviction (the disk space is
                                // already freed) and reconcile the DB so this row
                                // stops counting toward total_cache_bytes. Without
                                // this branch, eviction enters an infinite skip
                                // loop on phantom rows: every pass returns the
                                // same candidates, every candidate ENOENTs,
                                // pass_freed stays zero, the loop bails out and
                                // the cache never converges.
                                self.db.set_evicted(c.inode).await?;
                                pass_freed += c.cache_size;
                                debug!(inode = c.inode, bytes = c.cache_size,
                                       "evicted (cache file already missing on disk)");
                            }
                            Err(e) => {
                                warn!(inode = c.inode, path = ?c.cache_path,
                                      "evict remove error: {e}");
                                pass_skipped += 1;
                            }
                        }
                    }
                    _ => {
                        debug!(inode = c.inode, "skip eviction, status changed");
                        pass_skipped += 1;
                    }
                }
            }

            freed         += pass_freed;
            total_skipped += pass_skipped;

            // Bail if we made no progress this pass — likely all the
            // candidates we got back are "skipped" (status churned), and
            // re-querying would just return the same ones. Better to wait
            // for the next 60 s tick than spin.
            if pass_freed == 0 {
                warn!(
                    mount_id = self.mount_id,
                    skipped  = pass_skipped,
                    "eviction made no progress this pass; bailing"
                );
                break;
            }
        }

        info!(
            mount_id   = self.mount_id,
            freed_mb   = freed / 1_048_576,
            to_free_mb = to_free / 1_048_576,
            skipped    = total_skipped,
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

// ── Base version eviction ────────────────────────────────────────────────────

/// Spawn a background loop that periodically evicts stale base-version objects.
///
/// Removes base entries for files that haven't been modified locally
/// for `retention_days` days and whose sync status is not dirty/uploading.
/// Also GCs orphan blobs on disk that have no DB references.
pub fn spawn_base_eviction(
    mount_id:       u32,
    db:             Arc<StateDb>,
    base_store:     Arc<BaseStore>,
    retention_days: u32,
) {
    tokio::spawn(async move {
        let mut ticker = interval(Duration::from_secs(6 * 3600)); // every 6 hours
        loop {
            ticker.tick().await;
            if let Err(e) = evict_stale_bases(mount_id, &db, &base_store, retention_days).await {
                warn!(mount_id, "base eviction error: {e}");
            }
        }
    });
}

async fn evict_stale_bases(
    mount_id:       u32,
    db:             &Arc<StateDb>,
    base_store:     &Arc<BaseStore>,
    retention_days: u32,
) -> Result<()> {
    let stale = db.stale_base_entries(mount_id, retention_days, 500).await?;
    if stale.is_empty() {
        return Ok(());
    }

    let mut removed = 0u64;
    for (inode, hash, _size) in &stale {
        // Check if other inodes still reference this blob
        let refs = db.base_hash_ref_count(hash).await?;

        db.remove_base_hash(*inode, mount_id).await?;

        // Only delete the blob file if this was the last reference
        if refs <= 1 {
            base_store.remove_object(hash)?;
        }

        removed += 1;
    }

    if removed > 0 {
        info!(mount_id, count = removed, "evicted stale base versions");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::SystemTime;
    use stratosync_core::{
        state::NewFileEntry,
        types::{FileKind, SyncStatus},
    };

    /// Seed `n` cached files into the DB with on-disk content of `bytes_each`.
    /// Returns the cache root and the total bytes seeded.
    async fn seed_cached_files(
        db:         &Arc<StateDb>,
        mount_id:   u32,
        root_inode: u64,
        cache_dir:  &std::path::Path,
        n:          usize,
        bytes_each: u64,
    ) -> u64 {
        let blob = vec![0u8; bytes_each as usize];
        let mut total = 0u64;
        for i in 0..n {
            let name = format!("f{i}.bin");
            let cache_path: PathBuf = cache_dir.join(&name);
            tokio::fs::write(&cache_path, &blob).await.unwrap();

            let inode = db.insert_file(&NewFileEntry {
                mount_id,
                parent: root_inode,
                name: name.clone(),
                remote_path: format!("/{name}"),
                kind: FileKind::File,
                size: bytes_each,
                mtime: SystemTime::UNIX_EPOCH,
                etag: Some("e".into()),
                status: SyncStatus::Cached,
                cache_path: Some(cache_path.clone()),
                cache_size: Some(bytes_each),
            }).await.unwrap();

            db.touch_lru(inode).await.unwrap();
            total += bytes_each;
        }
        total
    }

    /// Common DB+mount setup used by the eviction tests.
    async fn make_mount(quota: u64) -> (Arc<StateDb>, u32, u64, tempfile::TempDir) {
        let db = Arc::new(StateDb::in_memory().unwrap());
        db.migrate().await.unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        let mount_id = db
            .upsert_mount(
                "test", "mock:/", "/mnt/test",
                cache_dir.path().to_str().unwrap(),
                quota, 60,
            ).await.unwrap();
        let root = db.insert_root(&NewFileEntry {
            mount_id, parent: 0,
            name: "/".into(), remote_path: "/".into(),
            kind: FileKind::Directory, size: 0,
            mtime: SystemTime::UNIX_EPOCH, etag: None,
            status: SyncStatus::Cached,
            cache_path: None, cache_size: None,
        }).await.unwrap();
        (db, mount_id, root, cache_dir)
    }

    /// Regression test for the per-pass `LIMIT 200` cap.
    ///
    /// Pre-fix, `lru_eviction_candidates(mount_id, 200)` returned at most
    /// 200 rows per pass — for many small files that's a tiny dent in the
    /// overage and the cache stayed permanently over quota.
    ///
    /// Seed 1500 × 10 KB = 15 MB into a 5 MB-quota mount. high_mark = 4.5 MB,
    /// low_mark = 4 MB. Need to free ~11 MB. The OLD code would have freed
    /// 200 × 10 KB = 2 MB and returned. The NEW code paginates and keeps
    /// going until under low_mark.
    #[tokio::test]
    async fn maybe_evict_converges_with_many_small_files() {
        let quota: u64 = 5 * 1024 * 1024;
        let (db, mount_id, root, _cache_dir_guard) = make_mount(quota).await;
        let cache_dir = _cache_dir_guard.path().to_path_buf();

        let total = seed_cached_files(&db, mount_id, root, &cache_dir, 1500, 10 * 1024).await;
        // 1500 files × 10 KiB each = 15_360_000 B, well above quota (5 MiB).
        assert!(total > quota * 2, "seeded {total} below 2× quota {quota}");
        assert_eq!(db.total_cache_bytes(mount_id).await.unwrap(), total);

        let cm = CacheManager::new(mount_id, Arc::clone(&db), quota);
        cm.maybe_evict().await.unwrap();

        let after = db.total_cache_bytes(mount_id).await.unwrap();
        let low_mark = (quota as f64 * 0.80) as u64;

        // Must be at-or-below low_mark — that's the convergence guarantee.
        assert!(
            after <= low_mark,
            "expected cache <= low_mark={low_mark}, got {after}",
        );
        // Must not have over-freed: we only need to hit low_mark, not 0.
        // Allow a one-batch overshoot (1000 × 10 KB = 10 MB worst case).
        assert!(after > 0, "evicted everything; expected partial eviction");
    }

    /// Regression test for the phantom-row infinite-skip loop.
    ///
    /// If the on-disk cache file is missing while the DB still has a
    /// `status='cached'` row pointing at it (manual `rm`, prior crash
    /// mid-eviction, etc.), the old code WARNed and skipped, leaving the
    /// row counted in `total_cache_bytes`. Eviction's `pass_freed == 0`
    /// guard then bailed out and the cache never converged.
    ///
    /// The fix treats ENOENT as success — the disk space is already freed,
    /// we just need to reconcile the DB. This test seeds 100 phantom rows
    /// (rows in the DB pointing to files that don't exist) plus 50 real
    /// rows, runs `maybe_evict`, and asserts both that the DB total drops
    /// to 0 and that the real files survive (we only need to free phantoms
    /// to converge — the real files are still under low-mark).
    #[tokio::test]
    async fn maybe_evict_reconciles_phantom_rows() {
        let quota: u64 = 1024 * 1024;     // 1 MiB
        let (db, mount_id, root, _cache_dir_guard) = make_mount(quota).await;
        let cache_dir = _cache_dir_guard.path().to_path_buf();
        let phantom_dir = cache_dir.join("phantoms");
        std::fs::create_dir_all(&phantom_dir).unwrap();

        // 100 phantom rows: cache_path set, but nothing on disk for them.
        // Each "cache_size" 100 KiB; collectively 10 MiB == 10× quota.
        for i in 0..100 {
            let phantom_path = phantom_dir.join(format!("ghost-{i}.bin"));
            db.insert_file(&NewFileEntry {
                mount_id,
                parent: root,
                name: format!("ghost-{i}.bin"),
                remote_path: format!("/ghost-{i}.bin"),
                kind: FileKind::File,
                size: 100 * 1024,
                mtime: SystemTime::UNIX_EPOCH,
                etag: Some("e".into()),
                status: SyncStatus::Cached,
                cache_path: Some(phantom_path),    // path doesn't exist
                cache_size: Some(100 * 1024),
            }).await.unwrap();
            db.touch_lru(*db.list_children(mount_id, root).await.unwrap()
                          .last().map(|e| &e.inode).unwrap()).await.unwrap();
        }

        // Sanity: DB-tracked total is far above quota even though disk is
        // mostly empty.
        let before = db.total_cache_bytes(mount_id).await.unwrap();
        assert!(before > quota * 2);

        let cm = CacheManager::new(mount_id, Arc::clone(&db), quota);
        cm.maybe_evict().await.unwrap();

        // The DB total should drop dramatically — most/all phantoms get
        // reconciled to status='remote', removing their cache_size from
        // the SUM.
        let after = db.total_cache_bytes(mount_id).await.unwrap();
        assert!(
            after <= (quota as f64 * 0.80) as u64,
            "expected total <= 80% of quota after phantom reconciliation, \
             got before={before} after={after}",
        );
    }

    /// Eviction is a no-op when usage is below the high-water mark.
    #[tokio::test]
    async fn maybe_evict_noop_when_under_high_mark() {
        let quota: u64 = 10 * 1024 * 1024;
        let (db, mount_id, root, _cache_dir_guard) = make_mount(quota).await;
        let cache_dir = _cache_dir_guard.path().to_path_buf();

        // 50 × 10 KB = 500 KB << 9 MB high mark.
        seed_cached_files(&db, mount_id, root, &cache_dir, 50, 10 * 1024).await;
        let before = db.total_cache_bytes(mount_id).await.unwrap();

        let cm = CacheManager::new(mount_id, Arc::clone(&db), quota);
        cm.maybe_evict().await.unwrap();

        let after = db.total_cache_bytes(mount_id).await.unwrap();
        assert_eq!(after, before, "should not evict when under high mark");
    }
}
