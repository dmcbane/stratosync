//! Startup orphan reconciler.
//!
//! Walks the per-mount cache directory and removes regular files that have
//! no corresponding `file_index.cache_path` entry in the DB. These orphans
//! accumulate over time — every code path where a cache file is created
//! out-of-band, the daemon crashes mid-cleanup, or a path renames without
//! the DB tracking the move leaves a file on disk that no DB row can ever
//! reach. Eviction can't touch them (it walks the LRU view of the DB), so
//! they leak disk space indefinitely.
//!
//! On a live daemon observed at 24 GB on disk vs. 8 GB in DB after the
//! eviction fixes landed, this reclaimed ~14 GB.
//!
//! We **do not** descend into:
//!  - `.bases/` — content-addressed BaseStore, managed separately by
//!    `cache::evict_stale_bases`.
//!  - `.meta/` — daemon-internal scratch (excluding `.meta/partial/`,
//!    handled separately below).
//!
//! We **do** unconditionally wipe `.meta/partial/` because every file in
//! there is a hydration temp file: it was created by `do_hydrate` during
//! a download, and the success path renames it onto its final
//! cache_path. Files left behind in `.meta/partial/` are by construction
//! orphans — the daemon crashed, the rename failed, the download stalled
//! and was retried elsewhere, or the user `kill -9`'d. Whatever the
//! reason, they leak disk space (one observed mount had 14 GiB of stale
//! partials) and cannot be recovered into a useful cache file because
//! they were never associated with a `cache_path`.
//!
//! The reconciler runs once at daemon startup, before FUSE mounts. Open
//! files inside the FUSE mount cannot race with us because the mount
//! doesn't exist yet.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use tracing::{debug, info, warn};

use stratosync_core::state::StateDb;

#[derive(Debug, Default, Clone, Copy)]
pub struct ReconcileStats {
    pub files_examined:  u64,
    pub orphans_removed: u64,
    pub partials_wiped:  u64,
    pub bytes_freed:     u64,
    pub remove_errors:   u64,
}

/// Walk `cache_dir` recursively and unlink any regular file whose path is
/// not in the DB's `cache_path` set.
pub async fn reconcile_orphans(
    mount_id:  u32,
    db:        &Arc<StateDb>,
    cache_dir: &Path,
) -> Result<ReconcileStats> {
    if !cache_dir.exists() {
        // Fresh mount, nothing to reconcile.
        return Ok(ReconcileStats::default());
    }

    let known = db.all_cache_paths(mount_id).await
        .context("loading known cache paths from DB")?;
    debug!(mount_id, known_count = known.len(), "starting orphan reconciliation");

    let stats = tokio::task::spawn_blocking({
        let cache_dir = cache_dir.to_path_buf();
        move || {
            let mut s = walk_and_remove_orphans(&cache_dir, &known);
            wipe_partial_hydrations(&cache_dir.join(".meta").join("partial"), &mut s);
            s
        }
    }).await.context("orphan reconciler panicked")?;

    if stats.orphans_removed + stats.partials_wiped > 0 || stats.remove_errors > 0 {
        info!(
            mount_id,
            examined        = stats.files_examined,
            orphans_removed = stats.orphans_removed,
            partials_wiped  = stats.partials_wiped,
            freed_mb        = stats.bytes_freed / 1_048_576,
            errors          = stats.remove_errors,
            "orphan reconciliation complete",
        );
    } else {
        debug!(
            mount_id,
            examined = stats.files_examined,
            "orphan reconciliation: cache is clean",
        );
    }
    Ok(stats)
}

/// Wipe every regular file under `.meta/partial/`. These are hydration
/// temp files; if they exist at startup, the daemon was killed before
/// `do_hydrate` could rename them onto their final cache_path. They are
/// orphans by construction and cannot be salvaged.
fn wipe_partial_hydrations(partial_dir: &Path, stats: &mut ReconcileStats) {
    if !partial_dir.exists() {
        return;
    }
    let entries = match std::fs::read_dir(partial_dir) {
        Ok(e) => e,
        Err(e) => {
            warn!(?partial_dir, "read_dir partial failed: {e}");
            return;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let ft = match entry.file_type() {
            Ok(t) => t,
            Err(e) => {
                warn!(?path, "file_type() failed: {e}");
                continue;
            }
        };
        if !ft.is_file() {
            continue;   // be conservative; only unlink regular files
        }
        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
        match std::fs::remove_file(&path) {
            Ok(()) => {
                stats.partials_wiped += 1;
                stats.bytes_freed    += size;
                debug!(?path, size, "wiped stale partial hydration");
            }
            Err(e) => {
                warn!(?path, "remove partial failed: {e}");
                stats.remove_errors += 1;
            }
        }
    }
}

/// Pure-blocking helper. Recursive but bounded by disk depth, no symlink
/// following (we never create symlinks in the cache, and following them
/// would be a security bug anyway).
fn walk_and_remove_orphans(
    cache_dir: &Path,
    known:     &HashSet<PathBuf>,
) -> ReconcileStats {
    let mut stats = ReconcileStats::default();
    let mut stack: Vec<PathBuf> = vec![cache_dir.to_path_buf()];

    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) => {
                warn!(?dir, "read_dir failed during reconcile: {e}");
                continue;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            // symlink_metadata() so we don't follow symlinks. The cache
            // tree should never contain any, but defence in depth.
            let md = match entry.file_type() {
                Ok(t)  => t,
                Err(e) => {
                    warn!(?path, "file_type() failed: {e}");
                    continue;
                }
            };

            if md.is_dir() {
                if is_reserved_subdir(&path, cache_dir) {
                    continue;     // skip .bases / .meta entirely
                }
                stack.push(path);
                continue;
            }
            if !md.is_file() {
                continue;         // symlink, fifo, socket — leave alone
            }

            stats.files_examined += 1;
            if known.contains(&path) {
                continue;
            }

            // Orphan. Stat for size, then unlink.
            let size = entry.metadata()
                            .map(|m| m.len())
                            .unwrap_or(0);
            match std::fs::remove_file(&path) {
                Ok(()) => {
                    stats.orphans_removed += 1;
                    stats.bytes_freed     += size;
                    debug!(?path, size, "removed orphan cache file");
                }
                Err(e) => {
                    warn!(?path, "remove orphan failed: {e}");
                    stats.remove_errors += 1;
                }
            }
        }
    }
    stats
}

fn is_reserved_subdir(path: &Path, cache_root: &Path) -> bool {
    // Match anything directly under cache_root named `.bases` or `.meta`.
    // Nested `.bases` / `.meta` directories under user paths are NOT
    // reserved — they're real user directories that happen to share a
    // name. We only skip the daemon's own at the cache root.
    let parent = match path.parent() {
        Some(p) => p,
        None    => return false,
    };
    if parent != cache_root {
        return false;
    }
    let name = path.file_name().and_then(|n| n.to_str());
    matches!(name, Some(".bases") | Some(".meta"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;
    use stratosync_core::{
        state::NewFileEntry,
        types::{FileKind, SyncStatus},
    };

    /// Builds a temp cache dir with a known + an orphan file, plus a
    /// `.bases` and `.meta` subtree containing decoy files that must NOT
    /// be removed.
    async fn fixture() -> (Arc<StateDb>, u32, tempfile::TempDir) {
        let db = Arc::new(StateDb::in_memory().unwrap());
        db.migrate().await.unwrap();
        let cache = tempfile::tempdir().unwrap();
        let mount_id = db.upsert_mount(
            "t", "mock:/", "/mnt/t",
            cache.path().to_str().unwrap(),
            5 << 30, 60,
        ).await.unwrap();
        let root = db.insert_root(&NewFileEntry {
            mount_id, parent: 0,
            name: "/".into(), remote_path: "/".into(),
            kind: FileKind::Directory, size: 0,
            mtime: SystemTime::UNIX_EPOCH, etag: None,
            status: SyncStatus::Cached,
            cache_path: None, cache_size: None,
        }).await.unwrap();

        // Known file: tracked in DB and on disk.
        let known_path = cache.path().join("kept.bin");
        std::fs::write(&known_path, b"hello").unwrap();
        db.insert_file(&NewFileEntry {
            mount_id, parent: root,
            name: "kept.bin".into(), remote_path: "/kept.bin".into(),
            kind: FileKind::File, size: 5,
            mtime: SystemTime::UNIX_EPOCH, etag: None,
            status: SyncStatus::Cached,
            cache_path: Some(known_path),
            cache_size: Some(5),
        }).await.unwrap();

        // Orphan files at root and in a nested user directory.
        std::fs::write(cache.path().join("ghost-root.bin"), b"orphan").unwrap();
        std::fs::create_dir_all(cache.path().join("subdir/nested")).unwrap();
        std::fs::write(cache.path().join("subdir/nested/ghost-deep.bin"),
                       vec![0u8; 1024]).unwrap();

        // Decoy: BaseStore-managed area MUST be left alone.
        std::fs::create_dir_all(cache.path().join(".bases/objects/aa")).unwrap();
        std::fs::write(cache.path().join(".bases/objects/aa/zz.blob"),
                       b"protected").unwrap();

        // Decoy: daemon scratch area MUST be left alone.
        std::fs::create_dir_all(cache.path().join(".meta/partial")).unwrap();
        std::fs::write(cache.path().join(".meta/partial/123.tmp"),
                       b"protected").unwrap();

        (db, mount_id, cache)
    }

    #[tokio::test]
    async fn removes_orphans_keeps_known_and_bases_wipes_partials() {
        let (db, mount_id, cache) = fixture().await;

        let stats = reconcile_orphans(mount_id, &db, cache.path())
            .await.unwrap();

        // 1 known + 2 orphans walked at the user level.
        assert_eq!(stats.files_examined, 3, "stats: {stats:?}");
        assert_eq!(stats.orphans_removed, 2, "stats: {stats:?}");
        // The .meta/partial decoy is also wiped (always-orphan by
        // construction — see module-level docs).
        assert_eq!(stats.partials_wiped, 1, "stats: {stats:?}");
        assert_eq!(stats.remove_errors, 0);

        // Tracked file survives.
        assert!(cache.path().join("kept.bin").exists());

        // Orphans are gone.
        assert!(!cache.path().join("ghost-root.bin").exists());
        assert!(!cache.path().join("subdir/nested/ghost-deep.bin").exists());

        // BaseStore-managed area untouched.
        assert!(cache.path().join(".bases/objects/aa/zz.blob").exists());

        // Stale partial hydration is gone (the wipe path wiped it).
        assert!(!cache.path().join(".meta/partial/123.tmp").exists());
        // The partial dir itself remains so do_hydrate can write into it.
        assert!(cache.path().join(".meta/partial").is_dir());
    }

    #[tokio::test]
    async fn empty_cache_dir_is_a_noop() {
        let db = Arc::new(StateDb::in_memory().unwrap());
        db.migrate().await.unwrap();
        let cache = tempfile::tempdir().unwrap();
        let mount_id = db.upsert_mount(
            "t", "mock:/", "/mnt/t",
            cache.path().to_str().unwrap(),
            5 << 30, 60,
        ).await.unwrap();

        let stats = reconcile_orphans(mount_id, &db, cache.path())
            .await.unwrap();
        assert_eq!(stats.files_examined, 0);
        assert_eq!(stats.orphans_removed, 0);
    }

    #[tokio::test]
    async fn missing_cache_dir_returns_default_stats() {
        let db = Arc::new(StateDb::in_memory().unwrap());
        db.migrate().await.unwrap();
        let cache = tempfile::tempdir().unwrap();
        let mount_id = db.upsert_mount(
            "t", "mock:/", "/mnt/t",
            cache.path().to_str().unwrap(),
            5 << 30, 60,
        ).await.unwrap();

        let nonexistent = cache.path().join("never-created");
        let stats = reconcile_orphans(mount_id, &db, &nonexistent)
            .await.unwrap();
        assert_eq!(stats.files_examined, 0);
        assert_eq!(stats.orphans_removed, 0);
    }

    #[test]
    fn nested_user_dirs_named_bases_or_meta_are_NOT_reserved() {
        // is_reserved_subdir only matches direct children of cache_root.
        let cache = std::path::Path::new("/cache/gdrive");
        assert!(is_reserved_subdir(
            std::path::Path::new("/cache/gdrive/.bases"), cache));
        assert!(is_reserved_subdir(
            std::path::Path::new("/cache/gdrive/.meta"), cache));
        // User happens to have a directory named ".bases" inside their
        // synced tree — should not be skipped.
        assert!(!is_reserved_subdir(
            std::path::Path::new("/cache/gdrive/Documents/.bases"), cache));
        assert!(!is_reserved_subdir(
            std::path::Path::new("/cache/gdrive/Projects/.meta"), cache));
    }
}
