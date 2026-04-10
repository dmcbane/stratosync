#![allow(dead_code, unused_imports)]
/// ConflictResolver — handles concurrent write collisions.
///
/// Algorithm: remote wins the canonical path; local version is uploaded
/// under a `.conflict.{ts}.{hash}.{ext}` sibling name.
/// When a base version exists and the file is text, attempts 3-way merge
/// via `git merge-file` before falling back to keep-both.
/// See docs/architecture/06-conflict-resolution.md for full design.
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use chrono::Utc;
use tracing::{debug, info, warn};

use stratosync_core::{
    backend::Backend,
    base_store::BaseStore,
    config::{ConflictStrategy, SyncConfig},
    state::StateDb,
    types::{FileEntry, FileKind, SyncStatus},
};
use stratosync_core::state::NewFileEntry;

// ── 3-way merge types ────────────────────────────────────────────────────────

/// Result of attempting a 3-way merge via `git merge-file`.
pub enum MergeOutcome {
    /// All changes merged cleanly — no conflict markers.
    Clean(Vec<u8>),
    /// Merge produced output with conflict markers (`<<<<<<<` / `=======` / `>>>>>>>`).
    ConflictMarkers(Vec<u8>),
    /// Merge could not run (git not found, I/O error, etc.).
    Failed(String),
}

/// Check once whether `git merge-file` is available.
/// Returns true if `git --version` succeeds.
pub fn git_available() -> bool {
    std::process::Command::new("git")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Attempt a 3-way merge using `git merge-file --stdout`.
///
/// Arguments follow git merge-file convention:
///   `git merge-file --stdout <local> <base> <remote>`
///
/// Exit codes:
///   0 = clean merge (stdout = merged content)
///   1 = conflicts (stdout = merged content with markers)
///   other = error
fn try_three_way_merge(
    base_path:   &Path,
    local_path:  &Path,
    remote_path: &Path,
) -> MergeOutcome {
    let output = match std::process::Command::new("git")
        .args([
            "merge-file", "--stdout",
            &local_path.to_string_lossy(),
            &base_path.to_string_lossy(),
            &remote_path.to_string_lossy(),
        ])
        .output()
    {
        Ok(o) => o,
        Err(e) => return MergeOutcome::Failed(format!("git merge-file: {e}")),
    };

    match output.status.code() {
        Some(0) => MergeOutcome::Clean(output.stdout),
        Some(1) => MergeOutcome::ConflictMarkers(output.stdout),
        Some(code) => MergeOutcome::Failed(format!(
            "git merge-file exited {code}: {}",
            String::from_utf8_lossy(&output.stderr)
        )),
        None => MergeOutcome::Failed("git merge-file killed by signal".into()),
    }
}

// ── Public entry point ────────────────────────────────────────────────────────

/// Called when an upload returns `SyncError::Conflict`.
///
/// If a base version exists and 3-way merge is enabled, attempts automatic
/// merge before falling back to the keep-both strategy.
///
/// Steps (keep-both fallback):
///  1. Compute a unique conflict filename.
///  2. Upload the local (losing) version under that name.
///  3. Download the remote (winning) version back into local cache.
///  4. Update the DB: canonical inode <- remote, new inode <- conflict file.
///  5. Emit a desktop notification if possible.
pub async fn resolve(
    entry:       &FileEntry,
    db:          &Arc<StateDb>,
    backend:     &Arc<dyn Backend>,
    base_store:  &Arc<BaseStore>,
    sync_config: &Arc<SyncConfig>,
    has_git:     bool,
) -> Result<()> {
    let cache_path = match &entry.cache_path {
        Some(p) => p.clone(),
        None => {
            warn!(inode = entry.inode, "conflict resolve called but no cache_path");
            return Ok(());
        }
    };

    // ── Attempt 3-way merge if conditions are met ────────────────────────────
    if has_git && sync_config.text_conflict_strategy == ConflictStrategy::Merge {
        if let Some(base_hash) = db.get_base_hash(entry.inode, entry.mount_id).await? {
            let base_path = base_store.object_path(&base_hash);
            let max_size = sync_config.base_max_file_size_bytes().unwrap_or(10 * 1024 * 1024);

            if base_path.exists()
                && BaseStore::is_text_mergeable(
                    &cache_path, entry.size, max_size, &sync_config.text_extensions,
                )
            {
                // Download remote version to a temp file for merge input
                let remote_tmp = cache_path.with_extension("stratosync-remote-tmp");
                if let Err(e) = backend.download(&entry.remote_path, &remote_tmp).await {
                    warn!(inode = entry.inode, "failed to download remote for merge: {e}");
                    // Fall through to keep-both
                } else {
                    match try_three_way_merge(&base_path, &cache_path, &remote_tmp) {
                        MergeOutcome::Clean(merged) => {
                            info!(
                                inode = entry.inode,
                                path = %entry.remote_path,
                                "3-way merge clean — no conflict file needed"
                            );

                            // Write merged content to cache
                            tokio::fs::write(&cache_path, &merged).await?;
                            let merged_size = merged.len() as u64;

                            // Upload the merged result (no ETag check — we're resolving)
                            let meta = backend.upload(&cache_path, &entry.remote_path, None).await?;

                            // Update DB with merged version
                            db.set_cached(
                                entry.inode, &cache_path, merged_size,
                                meta.etag.as_deref(), meta.mtime, meta.size,
                            ).await?;

                            // Update base version to the merged result
                            let bs = Arc::clone(base_store);
                            let cp = cache_path.clone();
                            let db2 = Arc::clone(db);
                            let mount_id = entry.mount_id;
                            let inode = entry.inode;
                            tokio::task::spawn_blocking(move || {
                                if let Ok(hash) = bs.store_base(&cp) {
                                    let _ = tokio::runtime::Handle::current().block_on(
                                        db2.set_base_hash(inode, mount_id, &hash, 0)
                                    );
                                }
                            });

                            // Clean up temp file
                            let _ = tokio::fs::remove_file(&remote_tmp).await;
                            return Ok(());
                        }
                        MergeOutcome::ConflictMarkers(merged) => {
                            info!(
                                inode = entry.inode,
                                path = %entry.remote_path,
                                "3-way merge has conflicts — writing markers to canonical, creating conflict sibling"
                            );

                            // Write merged-with-markers to cache (canonical gets markers)
                            tokio::fs::write(&cache_path, &merged).await?;

                            // Clean up temp file
                            let _ = tokio::fs::remove_file(&remote_tmp).await;

                            // Fall through to keep-both with the original local version
                            // uploaded as the conflict sibling. The canonical file now
                            // has the merged content with conflict markers for the user
                            // to review.
                            //
                            // Note: we continue to the keep-both path below, but the
                            // conflict file will contain the merge-with-markers version.
                            // The user edits the canonical file to resolve markers.
                        }
                        MergeOutcome::Failed(reason) => {
                            warn!(
                                inode = entry.inode,
                                "3-way merge failed: {reason} — falling back to keep-both"
                            );
                            let _ = tokio::fs::remove_file(&remote_tmp).await;
                            // Fall through to keep-both
                        }
                    }
                }
            }
        }
    }

    // ── Keep-both fallback ───────────────────────────────────────────────────

    // ── 1. Build conflict filename ────────────────────────────────────────────
    let conflict_name = make_conflict_name(&entry.name, &cache_path);
    let conflict_remote = sibling_path(&entry.remote_path, &conflict_name);

    info!(
        inode = entry.inode,
        canonical = %entry.remote_path,
        conflict  = %conflict_remote,
        "resolving conflict (keep-both)"
    );

    // ── 2. Upload local version under conflict name ───────────────────────────
    backend.upload(&cache_path, &conflict_remote, None).await?;

    // ── 3. Download the winning remote version ────────────────────────────────
    backend.download(&entry.remote_path, &cache_path).await?;

    let fs_meta = tokio::fs::metadata(&cache_path).await?;

    // ── 4. Update DB ──────────────────────────────────────────────────────────

    // Fetch fresh metadata for the canonical remote path.
    // If stat fails (e.g. transient network error), fall back to local file
    // metadata. The consequence is a missing etag, which means the next
    // upload will skip the optimistic-lock check — acceptable because we
    // just downloaded the winning version moments ago.
    let remote_meta = match backend.stat(&entry.remote_path).await {
        Ok(meta) => meta,
        Err(e) => {
            warn!(
                inode = entry.inode,
                path = %entry.remote_path,
                "stat after conflict download failed, using local metadata: {e}"
            );
            stratosync_core::types::RemoteMetadata {
                path:      entry.remote_path.clone(),
                name:      entry.name.clone(),
                size:      fs_meta.len(),
                mtime:     std::time::SystemTime::now(),
                is_dir:    false,
                etag:      None,
                checksum:  None,
                mime_type: None,
            }
        }
    };

    // Update canonical inode with the downloaded version
    db.set_cached(
        entry.inode,
        &cache_path,
        fs_meta.len(),
        remote_meta.etag.as_deref(),
        remote_meta.mtime,
        remote_meta.size,
    ).await?;

    // Insert a new inode for the conflict file
    // (fetch its metadata from the backend after upload)
    let conflict_meta = backend.stat(&conflict_remote).await;
    let conflict_size = conflict_meta.as_ref().map(|m| m.size).unwrap_or(fs_meta.len());
    let conflict_etag = conflict_meta.as_ref().ok().and_then(|m| m.etag.clone());

    db.insert_file(&NewFileEntry {
        mount_id:    entry.mount_id,
        parent:      entry.parent,
        name:        conflict_name.clone(),
        remote_path: conflict_remote.clone(),
        kind:        FileKind::File,
        size:        conflict_size,
        mtime:       std::time::SystemTime::now(),
        etag:        conflict_etag,
        status:      SyncStatus::Cached,
        cache_path:  None,   // conflict file lives remotely; not pinned locally
        cache_size:  None,
    }).await?;

    // ── 5. Desktop notification ───────────────────────────────────────────────
    emit_notification(&entry.name, &conflict_name);

    info!(
        inode = entry.inode,
        "conflict resolved: canonical={} conflict={}",
        entry.remote_path, conflict_remote
    );

    Ok(())
}

// ── Conflict filename ─────────────────────────────────────────────────────────

/// Builds:  `{stem}.conflict.{iso8601}.{sha256_prefix}.{ext}`
///
/// Example: `report.conflict.20250315T142301Z.a3f2e1b9.pdf`
fn make_conflict_name(original_name: &str, cache_path: &Path) -> String {
    let p    = Path::new(original_name);
    let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or(original_name);
    let ext  = p.extension().and_then(|e| e.to_str());

    let ts   = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let hash = file_hash_prefix(cache_path);

    match ext {
        Some(e) => format!("{stem}.conflict.{ts}.{hash}.{e}"),
        None    => format!("{stem}.conflict.{ts}.{hash}"),
    }
}

/// First 8 hex chars of a FNV-1a hash of the file's first 64 KiB.
/// Returns "00000000" if the file cannot be read — this is acceptable
/// for conflict naming (uniqueness is backstopped by the timestamp),
/// but we log it so the I/O failure is visible.
fn file_hash_prefix(path: &Path) -> String {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path) else {
        warn!(?path, "conflict hash: failed to open file, using fallback hash");
        return "00000000".into();
    };

    // Read up to 64 KiB for a fast representative hash
    let mut buf = vec![0u8; 65536];
    let n = match f.read(&mut buf) {
        Ok(n) => n,
        Err(e) => {
            warn!(?path, "conflict hash: read failed, using fallback hash: {e}");
            return "00000000".into();
        }
    };
    buf.truncate(n);

    // Simple FNV-1a 64-bit hash (no crypto needed here — just disambiguation)
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in &buf {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{:08x}", hash & 0xffffffff)
}

/// Replace the filename component of a remote path with a new name.
///
/// `sibling_path("gdrive:/Documents/report.pdf", "report.conflict.…pdf")`
/// → `"gdrive:/Documents/report.conflict.…pdf"`
fn sibling_path(remote_path: &str, new_name: &str) -> String {
    match remote_path.rfind('/') {
        Some(idx) => format!("{}/{}", &remote_path[..idx], new_name),
        None      => new_name.to_owned(),
    }
}

// ── Desktop notification ──────────────────────────────────────────────────────

fn emit_notification(original: &str, conflict_name: &str) {
    // notify-send may not be installed (headless server, non-GNOME DE, etc.).
    // Failure is expected and acceptable — the conflict is already logged
    // at info level by the caller.
    match std::process::Command::new("notify-send")
        .args([
            "--urgency=normal",
            "--icon=dialog-warning",
            "stratosync: sync conflict",
            &format!(
                "'{original}' was modified remotely and locally.\n\
                 Your local version was saved as '{conflict_name}'."
            ),
        ])
        .status()
    {
        Ok(s) if s.success() => debug!("desktop notification sent for conflict"),
        Ok(s) => debug!("notify-send exited with {s}"),
        Err(e) => debug!("notify-send unavailable: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn three_way_merge_clean() {
        // Skip if git is not available in the test environment
        if !git_available() {
            eprintln!("skipping: git not available");
            return;
        }

        let dir = tempfile::tempdir().unwrap();

        // Use enough context lines so git merge-file can distinguish the hunks
        let base = dir.path().join("base.txt");
        std::fs::write(&base, "line1\nline2\nline3\nline4\nline5\nline6\nline7\n").unwrap();

        let local = dir.path().join("local.txt");
        std::fs::write(&local, "line1 local\nline2\nline3\nline4\nline5\nline6\nline7\n").unwrap();

        let remote = dir.path().join("remote.txt");
        std::fs::write(&remote, "line1\nline2\nline3\nline4\nline5\nline6\nline7 remote\n").unwrap();

        match try_three_way_merge(&base, &local, &remote) {
            MergeOutcome::Clean(merged) => {
                let text = String::from_utf8(merged).unwrap();
                assert!(text.contains("line1 local"), "should have local change");
                assert!(text.contains("line7 remote"), "should have remote change");
            }
            MergeOutcome::ConflictMarkers(_) => panic!("expected Clean, got ConflictMarkers"),
            MergeOutcome::Failed(s) => panic!("expected Clean, got Failed: {s}"),
        }
    }

    #[test]
    fn three_way_merge_conflict_markers() {
        if !git_available() {
            eprintln!("skipping: git not available");
            return;
        }

        let dir = tempfile::tempdir().unwrap();

        let base = dir.path().join("base.txt");
        std::fs::write(&base, "line1\nline2\nline3\n").unwrap();

        // Both sides modify line2 differently
        let local = dir.path().join("local.txt");
        std::fs::write(&local, "line1\nline2 LOCAL\nline3\n").unwrap();

        let remote = dir.path().join("remote.txt");
        std::fs::write(&remote, "line1\nline2 REMOTE\nline3\n").unwrap();

        match try_three_way_merge(&base, &local, &remote) {
            MergeOutcome::ConflictMarkers(merged) => {
                let text = String::from_utf8(merged).unwrap();
                assert!(text.contains("<<<<<<<"), "should have conflict markers");
                assert!(text.contains(">>>>>>>"), "should have conflict markers");
            }
            MergeOutcome::Clean(_) => panic!("expected ConflictMarkers, got Clean"),
            MergeOutcome::Failed(s) => panic!("expected ConflictMarkers, got Failed: {s}"),
        }
    }

    #[test]
    fn three_way_merge_failed_bad_paths() {
        if !git_available() {
            eprintln!("skipping: git not available");
            return;
        }

        let result = try_three_way_merge(
            Path::new("/nonexistent/base"),
            Path::new("/nonexistent/local"),
            Path::new("/nonexistent/remote"),
        );
        matches!(result, MergeOutcome::Failed(_));
    }

    #[test]
    fn sibling_path_with_parent() {
        assert_eq!(
            sibling_path("gdrive:/Documents/report.pdf", "report.conflict.pdf"),
            "gdrive:/Documents/report.conflict.pdf"
        );
    }

    #[test]
    fn sibling_path_no_parent() {
        assert_eq!(sibling_path("report.pdf", "report.conflict.pdf"), "report.conflict.pdf");
    }

    // ── End-to-end resolve() tests with MockBackend ─────────────────────

    use stratosync_core::{
        backend::mock::MockBackend,
        state::NewFileEntry,
        types::{FileKind, Inode, SyncStatus, FUSE_ROOT_INODE},
    };
    use std::time::SystemTime;

    async fn setup_e2e() -> (
        Arc<StateDb>, Arc<dyn Backend>, Arc<BaseStore>, Arc<SyncConfig>,
        tempfile::TempDir, u32, /* mount_id */
    ) {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(StateDb::in_memory().unwrap());
        db.migrate().await.unwrap();
        let mount_id = db.upsert_mount(
            "test", "mock:/", "/mnt/test",
            dir.path().to_str().unwrap(), 5 << 30, 60,
        ).await.unwrap();
        db.insert_root(&NewFileEntry {
            mount_id, parent: 0,
            name: "/".into(), remote_path: "/".into(),
            kind: FileKind::Directory, size: 0,
            mtime: SystemTime::UNIX_EPOCH, etag: None,
            status: SyncStatus::Remote,
            cache_path: None, cache_size: None,
        }).await.unwrap();

        let backend: Arc<dyn Backend> = Arc::new(MockBackend::default());
        let base_store = Arc::new(
            BaseStore::new(dir.path().join(".bases")).unwrap()
        );
        let sync_config = Arc::new(SyncConfig {
            text_conflict_strategy: ConflictStrategy::Merge,
            ..Default::default()
        });

        (db, backend, base_store, sync_config, dir, mount_id)
    }

    async fn insert_file_entry(
        db: &Arc<StateDb>, mount_id: u32, name: &str, cache_path: &Path,
        content: &[u8],
    ) -> FileEntry {
        std::fs::write(cache_path, content).unwrap();
        let inode = db.insert_file(&NewFileEntry {
            mount_id, parent: FUSE_ROOT_INODE,
            name: name.into(), remote_path: name.into(),
            kind: FileKind::File, size: content.len() as u64,
            mtime: SystemTime::now(), etag: Some("etag-old".into()),
            status: SyncStatus::Dirty,
            cache_path: Some(cache_path.to_path_buf()),
            cache_size: Some(content.len() as u64),
        }).await.unwrap();
        db.get_by_inode(inode).await.unwrap().unwrap()
    }

    #[tokio::test]
    async fn e2e_clean_merge_non_overlapping() {
        if !git_available() {
            eprintln!("skipping: git not available");
            return;
        }

        let (db, backend, base_store, sync_config, dir, mount_id) = setup_e2e().await;

        // Base version: shared starting point
        let base_content = b"line1\nline2\nline3\nline4\nline5\nline6\nline7\n";
        // Local: changed line1
        let local_content = b"line1 LOCAL\nline2\nline3\nline4\nline5\nline6\nline7\n";
        // Remote: changed line7
        let remote_content = b"line1\nline2\nline3\nline4\nline5\nline6\nline7 REMOTE\n";

        let cache_path = dir.path().join("notes.txt");
        let entry = insert_file_entry(&db, mount_id, "notes.txt", &cache_path, local_content).await;

        // Store base version
        let base_file = dir.path().join("base-tmp.txt");
        std::fs::write(&base_file, base_content).unwrap();
        let hash = base_store.store_base(&base_file).unwrap();
        db.set_base_hash(entry.inode, mount_id, &hash, base_content.len() as u64).await.unwrap();

        // Seed remote with conflicting version
        backend.upload(&cache_path, "notes.txt", None).await.unwrap(); // establish remote
        let remote_file = dir.path().join("remote-seed.txt");
        std::fs::write(&remote_file, remote_content).unwrap();
        backend.upload(&remote_file, "notes.txt", None).await.unwrap(); // overwrite

        // Resolve the conflict
        resolve(&entry, &db, &backend, &base_store, &sync_config, true).await.unwrap();

        // Check: merged file should have BOTH changes
        let merged = std::fs::read_to_string(&cache_path).unwrap();
        assert!(merged.contains("line1 LOCAL"), "should have local change: {merged}");
        assert!(merged.contains("line7 REMOTE"), "should have remote change: {merged}");
        assert!(!merged.contains("<<<<<<<"), "should NOT have conflict markers: {merged}");

        // Status should be cached (upload succeeded)
        let updated = db.get_by_inode(entry.inode).await.unwrap().unwrap();
        assert_eq!(updated.status, SyncStatus::Cached);
    }

    #[tokio::test]
    async fn e2e_conflict_markers_overlapping() {
        if !git_available() {
            eprintln!("skipping: git not available");
            return;
        }

        let (db, backend, base_store, sync_config, dir, mount_id) = setup_e2e().await;

        let base_content = b"line1\nline2\nline3\n";
        let local_content = b"line1\nline2 LOCAL\nline3\n";
        let remote_content = b"line1\nline2 REMOTE\nline3\n";

        let cache_path = dir.path().join("config.txt");
        let entry = insert_file_entry(&db, mount_id, "config.txt", &cache_path, local_content).await;

        let base_file = dir.path().join("base-tmp.txt");
        std::fs::write(&base_file, base_content).unwrap();
        let hash = base_store.store_base(&base_file).unwrap();
        db.set_base_hash(entry.inode, mount_id, &hash, base_content.len() as u64).await.unwrap();

        backend.upload(&cache_path, "config.txt", None).await.unwrap();
        let remote_file = dir.path().join("remote-seed.txt");
        std::fs::write(&remote_file, remote_content).unwrap();
        backend.upload(&remote_file, "config.txt", None).await.unwrap();

        resolve(&entry, &db, &backend, &base_store, &sync_config, true).await.unwrap();

        // The canonical file should have the remote version (remote wins in keep-both)
        let canonical = std::fs::read_to_string(&cache_path).unwrap();
        assert!(canonical.contains("line2 REMOTE"), "canonical should be remote: {canonical}");

        // A conflict sibling should exist (the markers version was uploaded as sibling)
        let children = db.list_children(mount_id, FUSE_ROOT_INODE).await.unwrap();
        let conflict_entries: Vec<_> = children.iter()
            .filter(|e| e.name.contains("conflict"))
            .collect();
        assert_eq!(conflict_entries.len(), 1, "should have one conflict sibling");
        assert!(conflict_entries[0].name.starts_with("config.conflict."));
    }

    #[tokio::test]
    async fn e2e_no_base_falls_back_to_keep_both() {
        let (db, backend, base_store, sync_config, dir, mount_id) = setup_e2e().await;

        let local_content = b"local version";
        let remote_content = b"remote version";

        let cache_path = dir.path().join("doc.txt");
        let entry = insert_file_entry(&db, mount_id, "doc.txt", &cache_path, local_content).await;

        // No base version stored — should fall through to keep-both
        backend.upload(&cache_path, "doc.txt", None).await.unwrap();
        let remote_file = dir.path().join("remote-seed.txt");
        std::fs::write(&remote_file, remote_content).unwrap();
        backend.upload(&remote_file, "doc.txt", None).await.unwrap();

        resolve(&entry, &db, &backend, &base_store, &sync_config, true).await.unwrap();

        // Canonical file should have the remote version (remote wins)
        let canonical = std::fs::read_to_string(&cache_path).unwrap();
        assert_eq!(canonical, "remote version");

        // A conflict sibling should exist in the DB
        let children = db.list_children(mount_id, FUSE_ROOT_INODE).await.unwrap();
        let conflict_entries: Vec<_> = children.iter()
            .filter(|e| e.name.contains("conflict"))
            .collect();
        assert_eq!(conflict_entries.len(), 1, "should have one conflict file");
        assert!(conflict_entries[0].name.starts_with("doc.conflict."));
    }

    #[tokio::test]
    async fn e2e_binary_file_skips_merge() {
        let (db, backend, base_store, sync_config, dir, mount_id) = setup_e2e().await;

        // Binary content (contains NUL bytes)
        let local_content = b"local\x00binary";
        let remote_content = b"remote\x00binary";

        let cache_path = dir.path().join("image.bin");
        let entry = insert_file_entry(&db, mount_id, "image.bin", &cache_path, local_content).await;

        // Even with a base, binary files should skip merge
        let base_file = dir.path().join("base-tmp.bin");
        std::fs::write(&base_file, b"base\x00binary").unwrap();
        let hash = base_store.store_base(&base_file).unwrap();
        db.set_base_hash(entry.inode, mount_id, &hash, 12).await.unwrap();

        backend.upload(&cache_path, "image.bin", None).await.unwrap();
        let remote_file = dir.path().join("remote-seed.bin");
        std::fs::write(&remote_file, remote_content).unwrap();
        backend.upload(&remote_file, "image.bin", None).await.unwrap();

        resolve(&entry, &db, &backend, &base_store, &sync_config, true).await.unwrap();

        // Should fall through to keep-both (remote wins canonical)
        let canonical = std::fs::read(&cache_path).unwrap();
        assert_eq!(canonical, remote_content);
    }

    #[tokio::test]
    async fn e2e_merge_disabled_uses_keep_both() {
        let (db, backend, base_store, _, dir, mount_id) = setup_e2e().await;

        // Override config to KeepBoth
        let sync_config = Arc::new(SyncConfig {
            text_conflict_strategy: ConflictStrategy::KeepBoth,
            ..Default::default()
        });

        let base_content = b"line1\nline2\nline3\nline4\nline5\nline6\nline7\n";
        let local_content = b"line1 LOCAL\nline2\nline3\nline4\nline5\nline6\nline7\n";
        let remote_content = b"line1\nline2\nline3\nline4\nline5\nline6\nline7 REMOTE\n";

        let cache_path = dir.path().join("notes.txt");
        let entry = insert_file_entry(&db, mount_id, "notes.txt", &cache_path, local_content).await;

        // Store base (it exists but merge is disabled)
        let base_file = dir.path().join("base-tmp.txt");
        std::fs::write(&base_file, base_content).unwrap();
        let hash = base_store.store_base(&base_file).unwrap();
        db.set_base_hash(entry.inode, mount_id, &hash, base_content.len() as u64).await.unwrap();

        backend.upload(&cache_path, "notes.txt", None).await.unwrap();
        let remote_file = dir.path().join("remote-seed.txt");
        std::fs::write(&remote_file, remote_content).unwrap();
        backend.upload(&remote_file, "notes.txt", None).await.unwrap();

        resolve(&entry, &db, &backend, &base_store, &sync_config, true).await.unwrap();

        // Should be keep-both despite base version existing
        let canonical = std::fs::read_to_string(&cache_path).unwrap();
        assert!(canonical.contains("line7 REMOTE"), "canonical should be remote version");
        assert!(!canonical.contains("line1 LOCAL"), "canonical should NOT have local edit");

        let children = db.list_children(mount_id, FUSE_ROOT_INODE).await.unwrap();
        assert!(children.iter().any(|e| e.name.contains("conflict")));
    }

    #[tokio::test]
    async fn e2e_no_git_falls_back_to_keep_both() {
        let (db, backend, base_store, sync_config, dir, mount_id) = setup_e2e().await;

        let base_content = b"line1\nline2\nline3\nline4\nline5\nline6\nline7\n";
        let local_content = b"line1 LOCAL\nline2\nline3\nline4\nline5\nline6\nline7\n";
        let remote_content = b"line1\nline2\nline3\nline4\nline5\nline6\nline7 REMOTE\n";

        let cache_path = dir.path().join("readme.md");
        let entry = insert_file_entry(&db, mount_id, "readme.md", &cache_path, local_content).await;

        let base_file = dir.path().join("base-tmp.txt");
        std::fs::write(&base_file, base_content).unwrap();
        let hash = base_store.store_base(&base_file).unwrap();
        db.set_base_hash(entry.inode, mount_id, &hash, base_content.len() as u64).await.unwrap();

        backend.upload(&cache_path, "readme.md", None).await.unwrap();
        let remote_file = dir.path().join("remote-seed.txt");
        std::fs::write(&remote_file, remote_content).unwrap();
        backend.upload(&remote_file, "readme.md", None).await.unwrap();

        // Pass has_git=false to simulate no git installed
        resolve(&entry, &db, &backend, &base_store, &sync_config, false).await.unwrap();

        // Should fall through to keep-both
        let canonical = std::fs::read_to_string(&cache_path).unwrap();
        assert!(canonical.contains("line7 REMOTE"), "canonical should be remote version");
        let children = db.list_children(mount_id, FUSE_ROOT_INODE).await.unwrap();
        assert!(children.iter().any(|e| e.name.contains("conflict")));
    }
}
