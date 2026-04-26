/// RemotePoller — detects remote changes and updates the local file index.
///
/// Supports two modes:
/// - **Full listing** (default): fetches the full remote listing via rclone,
///   diffs against the DB snapshot using ETags and a generation counter.
/// - **Delta (change token)**: uses provider-specific APIs (e.g. Google Drive
///   Changes) to fetch only what changed since the last poll.
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

use stratosync_core::{
    backend::Backend, base_store::BaseStore, ipc::PollerStatus,
    state::{StateDb, VersionSource}, types::*, GlobSet, RemoteMetadata,
};

pub struct RemotePoller {
    mount_id:      u32,
    db:            Arc<StateDb>,
    backend:       Arc<dyn Backend>,
    poll_interval: Duration,
    state:         Arc<RwLock<PollerStatus>>,
    ignore:        Arc<GlobSet>,
    base_store:    Arc<BaseStore>,
    /// File size cap for version snapshots — files larger than this are
    /// not snapshotted, to keep the version cache bounded.
    version_max_size:  u64,
    /// `0` disables versioning. Otherwise: keep this many history rows
    /// per inode.
    version_retention: u32,
}

impl RemotePoller {
    pub fn new(
        mount_id:      u32,
        db:            Arc<StateDb>,
        backend:       Arc<dyn Backend>,
        poll_interval: Duration,
        ignore:        Arc<GlobSet>,
        base_store:    Arc<BaseStore>,
        version_max_size:  u64,
        version_retention: u32,
    ) -> Self {
        let mode = if backend.supports_delta() { "delta" } else { "full-listing" };
        let state = Arc::new(RwLock::new(PollerStatus {
            mode: mode.to_string(),
            current_interval_secs: poll_interval.as_secs(),
            ..Default::default()
        }));
        Self {
            mount_id, db, backend, poll_interval, state, ignore,
            base_store, version_max_size, version_retention,
        }
    }

    /// Expose a handle to the live poller state so the IPC server can
    /// snapshot it without going through the poller's task.
    pub fn state_handle(&self) -> Arc<RwLock<PollerStatus>> {
        Arc::clone(&self.state)
    }

    /// Run the poll loop forever. Call from a dedicated tokio task.
    pub async fn run(self) {
        let use_delta = self.backend.supports_delta();
        let mode = if use_delta { "delta" } else { "full-listing" };
        info!(mount_id = self.mount_id, interval = ?self.poll_interval, mode, "remote poller started");

        let base_interval = self.poll_interval;
        let mut current_interval = base_interval;
        let mut consecutive_failures: u32 = 0;
        let mut first_run = true;

        loop {
            // Poll immediately on first run so the directory tree is
            // populated before the user navigates.
            if first_run {
                first_run = false;
            } else {
                tokio::time::sleep(current_interval).await;
            }
            let result = if use_delta {
                self.poll_once_delta().await
            } else {
                self.poll_once().await
            };
            match result {
                Ok(()) => {
                    if consecutive_failures > 0 {
                        info!(mount_id = self.mount_id, "poll recovered after {} failures", consecutive_failures);
                        current_interval = base_interval;
                        consecutive_failures = 0;
                    }
                    self.update_state(
                        true, None, current_interval, consecutive_failures,
                    ).await;
                }
                Err(e) => {
                    consecutive_failures += 1;
                    if consecutive_failures >= 10 {
                        error!(
                            mount_id = self.mount_id,
                            failures = consecutive_failures,
                            "remote sync halted after {consecutive_failures} consecutive failures; \
                             check rclone auth and network connectivity: {e}"
                        );
                    } else if consecutive_failures >= 3 {
                        // Exponential backoff: double interval, cap at 10 minutes
                        current_interval = (current_interval * 2).min(Duration::from_secs(600));
                        warn!(
                            mount_id = self.mount_id,
                            failures = consecutive_failures,
                            next_in = ?current_interval,
                            "poll failed, backing off: {e}"
                        );
                    } else {
                        warn!(mount_id = self.mount_id, "poll error: {e}");
                    }
                    self.update_state(
                        false, Some(format!("{e}")), current_interval, consecutive_failures,
                    ).await;
                }
            }
        }
    }

    /// Record a poll result (success or failure) into the shared state so
    /// the dashboard can display it.
    async fn update_state(
        &self,
        success: bool,
        error: Option<String>,
        interval: Duration,
        failures: u32,
    ) {
        let now = SystemTime::now().duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64).unwrap_or(0);
        let next = now + interval.as_secs() as i64;
        let mut s = self.state.write().await;
        s.last_poll_unix = Some(now);
        s.next_poll_unix = Some(next);
        s.consecutive_failures = failures;
        s.current_interval_secs = interval.as_secs();
        if success {
            s.last_error = None;
        } else {
            s.last_error = error;
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
        let mut skipped_ignored = 0usize;

        for meta in &remote_files {
            // Safety: skip path traversal and null bytes
            if meta.path.contains("..") || meta.path.contains('\0') || meta.name.contains('/') {
                skipped_unsafe += 1;
                continue;
            }

            // Skip conflict files stored under the conflict namespace.
            // These are tracked in the DB from creation and must not be
            // re-imported as regular files.
            if meta.path.starts_with(CONFLICT_PREFIX) {
                continue;
            }

            // Skip tombstoned paths
            if tombstones.iter().any(|t| meta.path == *t || meta.path.starts_with(&format!("{t}/"))) {
                skipped_tombstoned += 1;
                continue;
            }

            // Selective sync: never index entries matching ignore_patterns.
            // If an entry was already indexed before the pattern was added,
            // bump its generation so phase-3 delete_stale_entries doesn't
            // wipe it. Ignore rules prevent NEW indexing, never retroactively
            // unindex (matches gitignore behavior; reversible).
            if self.ignore.is_match(&meta.path) {
                if let Some(snap) = db_snapshot.get(&meta.path) {
                    unchanged_inodes.push(snap.inode);
                }
                skipped_ignored += 1;
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

        // ── Phase 2.5: Version-history snapshots ─────────────────────────────
        // Capture pre-replace snapshots for entries that are about to be
        // overwritten by a remote change. Only files currently in `Cached`
        // status have meaningful local content to preserve — Dirty/Uploading
        // produce conflict files via the existing conflict path, and
        // Hydrating/Remote have no committed local content yet.
        if self.version_retention > 0 {
            for meta in &to_upsert {
                let Some(snap) = db_snapshot.get(&meta.path) else { continue };
                if snap.status != SyncStatus::Cached { continue; }
                // Look up the cache_path for this inode. We don't have it in
                // the snapshot (only inode/etag/size/status are there); a
                // single-row fetch is fine — this branch is rare.
                let entry = match self.db.get_by_inode(snap.inode).await {
                    Ok(Some(e)) => e,
                    _ => continue,
                };
                let Some(cp) = entry.cache_path.as_deref() else { continue };
                if let Err(e) = super::versioning::capture(
                    &self.db, &self.base_store,
                    snap.inode, self.mount_id, cp,
                    entry.size, entry.etag.as_deref(),
                    VersionSource::BeforePoll,
                    self.version_max_size, self.version_retention,
                ).await {
                    warn!(inode = snap.inode, "version snapshot failed: {e}");
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

        // Mark all directories as listed — safe after a full recursive listing
        // since the complete tree is present in the DB. This prevents FUSE
        // readdir from redundantly calling backend.list() for every directory.
        match self.db.batch_mark_dirs_listed(self.mount_id).await {
            Ok(n) if n > 0 => debug!(n, "directories marked as listed after full poll"),
            Ok(_) => {}
            Err(e) => warn!("batch_mark_dirs_listed failed: {e}"),
        }

        debug!(
            mount_id    = self.mount_id,
            total       = remote_files.len(),
            changed     = to_upsert.len(),
            unchanged   = unchanged_inodes.len(),
            deleted     = deleted.len(),
            tombstoned  = skipped_tombstoned,
            ignored     = skipped_ignored,
            "poll complete"
        );
        Ok(())
    }

    /// Delta poll: fetch only changes since the last token.
    /// Falls back to full listing on first run or when the token expires.
    async fn poll_once_delta(&self) -> Result<()> {
        debug!(mount_id = self.mount_id, "polling remote (delta mode)");

        let token = self.db.get_change_token(self.mount_id).await?;

        let token = match token {
            Some(t) => t,
            None => {
                // No token yet — need initial full listing + start token.
                // Check if the DB already has entries (from a prior listing
                // that succeeded before get_start_token failed). If so,
                // skip the expensive listing and just get the token.
                let snap = self.db.snapshot_remote_index(self.mount_id).await?;
                if snap.len() <= 1 {
                    // Empty or root-only — try a full listing first.
                    // If the listing fails, proceed anyway: get a start token
                    // and let delta mode catch up incrementally. The index
                    // may be incomplete until a full listing succeeds.
                    info!(mount_id = self.mount_id, "no change token; running initial full listing");
                    if let Err(e) = self.poll_once().await {
                        warn!(
                            mount_id = self.mount_id,
                            "initial full listing failed ({e}); \
                             proceeding with delta-only mode — index may be incomplete"
                        );
                    }
                } else {
                    debug!(
                        mount_id = self.mount_id,
                        entries = snap.len(),
                        "DB already populated; skipping full listing"
                    );
                }
                let start = self.backend.get_start_token().await
                    .map_err(|e| anyhow::anyhow!("get_start_token: {e}"))?;
                self.db.set_change_token(self.mount_id, &start).await?;
                info!(mount_id = self.mount_id, "stored initial change token");
                return Ok(());
            }
        };

        // Fetch changes since the stored token
        let (changes, next_token) = match self.backend.changes_since(&token).await {
            Ok(result) => result,
            Err(SyncError::TokenExpired) => {
                warn!(mount_id = self.mount_id, "change token expired; falling back to full listing");
                self.db.clear_change_token(self.mount_id).await?;
                self.poll_once().await?;
                let start = self.backend.get_start_token().await
                    .map_err(|e| anyhow::anyhow!("get_start_token after fallback: {e}"))?;
                self.db.set_change_token(self.mount_id, &start).await?;
                return Ok(());
            }
            Err(e) => return Err(anyhow::anyhow!("changes_since: {e}")),
        };

        if changes.is_empty() {
            debug!(mount_id = self.mount_id, "delta poll: no changes");
            self.db.set_change_token(self.mount_id, &next_token).await?;
            return Ok(());
        }

        // Load tombstones for filtering
        let tombstones = self.db.active_tombstones(self.mount_id).await
            .unwrap_or_default();

        // Load current DB state for parent resolution
        let db_snapshot = self.db.snapshot_remote_index(self.mount_id).await?;
        let mut path_to_inode: HashMap<String, Inode> = db_snapshot.iter()
            .map(|(path, snap)| (path.clone(), snap.inode))
            .collect();

        let mut added = 0usize;
        let mut deleted = 0usize;

        for change in &changes {
            match change {
                RemoteChange::Added { meta } | RemoteChange::Modified { meta, .. } => {
                    // Skip conflict namespace
                    if meta.path.starts_with(CONFLICT_PREFIX) {
                        continue;
                    }

                    // Skip tombstoned paths
                    if tombstones.iter().any(|t| {
                        meta.path == *t || meta.path.starts_with(&format!("{t}/"))
                    }) {
                        continue;
                    }

                    // Safety: skip path traversal and null bytes
                    if meta.path.contains("..") || meta.path.contains('\0')
                        || meta.name.contains('/')
                    {
                        continue;
                    }

                    // Selective sync: never insert ignored entries. Delta
                    // mode has no generation sweep, so a plain skip is fine.
                    if self.ignore.is_match(&meta.path) {
                        continue;
                    }

                    // Versioning: snapshot pre-replace content if the
                    // existing entry was cached (same logic as full poll).
                    if self.version_retention > 0 {
                        if let Some(snap) = db_snapshot.get(&meta.path) {
                            if snap.status == SyncStatus::Cached {
                                if let Ok(Some(entry)) = self.db.get_by_inode(snap.inode).await {
                                    if let Some(cp) = entry.cache_path.as_deref() {
                                        if let Err(e) = super::versioning::capture(
                                            &self.db, &self.base_store,
                                            snap.inode, self.mount_id, cp,
                                            entry.size, entry.etag.as_deref(),
                                            VersionSource::BeforePoll,
                                            self.version_max_size, self.version_retention,
                                        ).await {
                                            warn!(inode = snap.inode, "version snapshot failed (delta): {e}");
                                        }
                                    }
                                }
                            }
                        }
                    }

                    let kind = if meta.is_dir {
                        FileKind::Directory
                    } else {
                        FileKind::File
                    };

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
                        0, // generation is not used in delta mode
                    ).await?;

                    path_to_inode.insert(meta.path.clone(), inode);
                    added += 1;
                }
                RemoteChange::Deleted { path } => {
                    if let Some((inode, cache_path)) = self.db
                        .delete_remote_entry_by_path(self.mount_id, path).await?
                    {
                        info!(inode, path = %path, "remote deletion detected (delta)");
                        if let Some(cp) = cache_path {
                            let _ = tokio::fs::remove_file(cp).await;
                        }
                        deleted += 1;
                    }
                }
            }
        }

        // Store next token
        self.db.set_change_token(self.mount_id, &next_token).await?;

        // Clean up expired tombstones
        if let Ok(cleaned) = self.db.cleanup_expired_tombstones().await {
            if cleaned > 0 {
                debug!(cleaned, "expired tombstones removed");
            }
        }

        debug!(
            mount_id = self.mount_id,
            total_changes = changes.len(),
            added,
            deleted,
            "delta poll complete"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stratosync_core::{
        backend::mock::MockBackend,
        state::NewFileEntry,
        types::{FileKind, SyncStatus},
        GlobSetBuilder,
    };

    async fn setup(patterns: &[&str]) -> (Arc<StateDb>, u32, MockBackend, Arc<GlobSet>) {
        let db = Arc::new(StateDb::in_memory().unwrap());
        db.migrate().await.unwrap();
        let mount_id = db
            .upsert_mount("test", "mock:/", "/mnt/test", "/tmp/cache", 5 << 30, 60)
            .await
            .unwrap();
        db.insert_root(&NewFileEntry {
            mount_id, parent: 0,
            name: "/".into(), remote_path: "/".into(),
            kind: FileKind::Directory, size: 0,
            mtime: SystemTime::UNIX_EPOCH, etag: None,
            status: SyncStatus::Remote,
            cache_path: None, cache_size: None,
        }).await.unwrap();

        let mut b = GlobSetBuilder::new();
        for p in patterns {
            b.add(stratosync_core::Glob::new(p).unwrap());
        }
        let ignore = Arc::new(b.build().unwrap());

        (db, mount_id, MockBackend::default(), ignore)
    }

    fn poller(
        mount_id: u32,
        db: Arc<StateDb>,
        backend: MockBackend,
        ignore: Arc<GlobSet>,
    ) -> RemotePoller {
        poller_with_versioning(mount_id, db, backend, ignore, 0)
    }

    fn poller_with_versioning(
        mount_id: u32,
        db: Arc<StateDb>,
        backend: MockBackend,
        ignore: Arc<GlobSet>,
        retention: u32,
    ) -> RemotePoller {
        let backend: Arc<dyn Backend> = Arc::new(backend);
        let bs_dir = tempfile::tempdir().unwrap().keep();
        let bs = Arc::new(BaseStore::new(bs_dir).unwrap());
        RemotePoller::new(
            mount_id, db, backend, Duration::from_secs(60), ignore,
            bs, 10 * 1024 * 1024, retention,
        )
    }

    #[tokio::test]
    async fn poll_skips_ignored_remote_files() {
        let (db, mid, backend, ignore) = setup(&["*.log", "node_modules/**"]).await;

        backend.seed_file("keep.txt", b"hi");
        backend.seed_file("noisy.log", b"trace");
        backend.seed_file("node_modules/lib.js", b"x");
        backend.seed_file("src/main.rs", b"fn main(){}");

        let p = poller(mid, Arc::clone(&db), backend, ignore);
        p.poll_once().await.unwrap();

        // Non-ignored files indexed
        assert!(db.get_by_remote_path(mid, "keep.txt").await.unwrap().is_some());
        assert!(db.get_by_remote_path(mid, "src/main.rs").await.unwrap().is_some());

        // Ignored files not indexed
        assert!(db.get_by_remote_path(mid, "noisy.log").await.unwrap().is_none());
        assert!(db.get_by_remote_path(mid, "node_modules/lib.js").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn poll_preserves_already_indexed_entries_that_newly_match() {
        // Regression guard: phase-3 delete_stale_entries would otherwise wipe
        // entries that we silently dropped from the upsert/unchanged sets.
        // Pre-seed the DB with a file, then add an ignore pattern that matches
        // it and run a poll. The entry must survive.

        let (db, mid, backend, _) = setup(&[]).await;

        // First pass: index legacy.log normally.
        backend.seed_file("legacy.log", b"old");
        backend.seed_file("kept.txt", b"keep");
        let p1 = {
            let b = backend.clone();
            poller(mid, Arc::clone(&db), b, Arc::new(GlobSet::empty()))
        };
        p1.poll_once().await.unwrap();
        let before = db.get_by_remote_path(mid, "legacy.log").await.unwrap();
        assert!(before.is_some(), "legacy.log should be indexed by first poll");
        let before_inode = before.unwrap().inode;

        // Second pass: ignore *.log. legacy.log must still be in the DB
        // (preserve-on-ignore), inode unchanged. kept.txt also still there.
        let mut b = GlobSetBuilder::new();
        b.add(stratosync_core::Glob::new("*.log").unwrap());
        let ignore = Arc::new(b.build().unwrap());
        let p2 = poller(mid, Arc::clone(&db), backend, ignore);
        p2.poll_once().await.unwrap();

        let after = db.get_by_remote_path(mid, "legacy.log").await.unwrap();
        assert!(after.is_some(), "legacy.log must NOT be wiped by stale-sweep");
        assert_eq!(after.unwrap().inode, before_inode, "inode must not change");
        assert!(db.get_by_remote_path(mid, "kept.txt").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn poll_captures_version_before_replacing_cached_file() {
        // First poll indexes the file. We then force the entry into Cached
        // state with a real cache file on disk, modify the remote (changing
        // size/etag), and re-poll. The pre-replace snapshot should land in
        // version_history and the blob should exist in BaseStore.
        let (db, mid, backend, ignore) = setup(&[]).await;

        // Stage the cache file ourselves so the entry is plausibly Cached.
        let cache_dir = tempfile::tempdir().unwrap();
        let cache_path = cache_dir.path().join("doc.txt");
        std::fs::write(&cache_path, b"the original content").unwrap();

        backend.seed_file("doc.txt", b"the original content");
        let bs_dir = tempfile::tempdir().unwrap().keep();
        let bs = Arc::new(BaseStore::new(bs_dir.clone()).unwrap());
        let backend_arc: Arc<dyn Backend> = Arc::new(backend.clone());
        let p = RemotePoller::new(
            mid, Arc::clone(&db), backend_arc, Duration::from_secs(60),
            Arc::clone(&ignore), Arc::clone(&bs), 10 * 1024 * 1024, 5,
        );
        p.poll_once().await.unwrap();

        // Mark the indexed entry as Cached with our staged cache file.
        let entry = db.get_by_remote_path(mid, "doc.txt").await.unwrap().unwrap();
        db.set_cached(entry.inode, &cache_path,
            "the original content".len() as u64,
            entry.etag.as_deref(), std::time::SystemTime::now(),
            "the original content".len() as u64,
        ).await.unwrap();

        // Remote changes — size & etag now differ.
        backend.modify_file("doc.txt", b"DIFFERENT REMOTE CONTENT NOW");
        p.poll_once().await.unwrap();

        // The OLD cache content should be in version_history.
        let history = db.list_version_history(entry.inode).await.unwrap();
        assert_eq!(history.len(), 1, "exactly one snapshot — pre-replace");
        assert_eq!(history[0].source, VersionSource::BeforePoll);
        assert_eq!(history[0].file_size, "the original content".len() as u64);

        // And the blob should exist on disk under the BaseStore.
        let blob_path = bs.object_path(&history[0].object_hash);
        assert!(blob_path.exists(), "version blob must be on disk");
        let body = std::fs::read(&blob_path).unwrap();
        assert_eq!(body, b"the original content");
    }

    #[tokio::test]
    async fn poll_skips_version_capture_when_retention_zero() {
        // With retention=0, the poller must NOT capture a snapshot even
        // when a Cached file is being replaced. (Default behavior for
        // mounts that haven't opted into versioning.)
        let (db, mid, backend, ignore) = setup(&[]).await;

        let cache_dir = tempfile::tempdir().unwrap();
        let cache_path = cache_dir.path().join("doc.txt");
        std::fs::write(&cache_path, b"original").unwrap();

        backend.seed_file("doc.txt", b"original");
        let p = poller_with_versioning(mid, Arc::clone(&db),
            backend.clone(), Arc::clone(&ignore), 0); // retention=0
        p.poll_once().await.unwrap();

        let entry = db.get_by_remote_path(mid, "doc.txt").await.unwrap().unwrap();
        db.set_cached(entry.inode, &cache_path, 8, entry.etag.as_deref(),
            std::time::SystemTime::now(), 8).await.unwrap();

        backend.modify_file("doc.txt", b"changed");
        p.poll_once().await.unwrap();

        let history = db.list_version_history(entry.inode).await.unwrap();
        assert!(history.is_empty(), "retention=0 must skip capture entirely");
    }

    #[tokio::test]
    async fn poll_does_not_re_index_ignored_after_remote_modification() {
        let (db, mid, backend, ignore) = setup(&["*.log"]).await;

        backend.seed_file("trace.log", b"v1");
        let p = poller(mid, Arc::clone(&db), backend.clone(), Arc::clone(&ignore));
        p.poll_once().await.unwrap();
        assert!(db.get_by_remote_path(mid, "trace.log").await.unwrap().is_none());

        backend.modify_file("trace.log", b"v2-now-bigger");
        p.poll_once().await.unwrap();
        assert!(
            db.get_by_remote_path(mid, "trace.log").await.unwrap().is_none(),
            "modification of an ignored remote must not insert it"
        );
    }
}
