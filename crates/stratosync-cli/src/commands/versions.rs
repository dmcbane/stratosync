//! `stratosync versions list <path>` and `stratosync versions restore <path> --index N`.
//!
//! Snapshots are produced by the daemon (poller pre-replace and post-upload)
//! into a content-addressed store under `<cache_dir>/.bases/objects/`. The
//! CLI reads the `version_history` table to list them and copies a blob back
//! to the cache file to restore. Restore marks the file Dirty so the daemon
//! re-uploads it through the normal write path.
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use bytesize::ByteSize;
use chrono::{Local, TimeZone};
use stratosync_core::{
    base_store::BaseStore,
    config::{default_data_dir, MountConfig},
    state::StateDb,
    types::{FileEntry, Inode},
};

pub async fn list(config_path: &Path, user_path: &Path) -> Result<()> {
    let ctx = resolve(config_path, user_path).await?;
    let history = ctx.db.list_version_history(ctx.entry.inode).await?;

    if history.is_empty() {
        println!("No versions recorded for '{}'.", ctx.entry.remote_path);
        if ctx.mount.version_retention == 0 {
            println!("Versioning is disabled for mount '{}'. Set `version_retention = 10` (or similar) in config.toml.",
                ctx.mount.name);
        }
        return Ok(());
    }

    println!("{:>3}  {:<19}  {:>10}  {:<13}  hash", "#", "recorded", "size", "source");
    for (i, v) in history.iter().enumerate() {
        println!("{:>3}  {:<19}  {:>10}  {:<13}  {}",
            i,
            format_ts(v.recorded_at),
            ByteSize(v.file_size).to_string(),
            v.source.as_str(),
            short_hash(&v.object_hash),
        );
    }
    println!();
    println!("To restore: stratosync versions restore '{}' --index <#>",
        user_path.display());
    Ok(())
}

pub async fn restore(
    config_path: &Path,
    user_path:   &Path,
    index:       usize,
) -> Result<()> {
    let ctx = resolve(config_path, user_path).await?;
    let history = ctx.db.list_version_history(ctx.entry.inode).await?;

    let v = history.get(index).ok_or_else(|| anyhow::anyhow!(
        "no version at index {index} for '{}' — file has {} historical versions (0..{})",
        ctx.entry.remote_path, history.len(),
        history.len().saturating_sub(1),
    ))?;

    // Locate the blob.
    let cache_dir = ctx.mount.cache_dir();
    let bs = BaseStore::new(cache_dir.join(".bases"))
        .context("opening base store")?;
    let blob = bs.object_path(&v.object_hash);
    anyhow::ensure!(
        blob.exists(),
        "blob {} for version #{} is missing on disk — was the cache evicted?",
        short_hash(&v.object_hash), index,
    );

    // Restore by copying the blob over the cache file. If the cache file
    // doesn't exist (file was never hydrated), create it; the daemon will
    // re-upload from this content.
    let cache_path = pick_or_synthesize_cache_path(&ctx.entry, &cache_dir);
    if let Some(parent) = cache_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating cache dir {parent:?}"))?;
    }
    install_restored_content(&ctx.db, ctx.entry.inode, &blob, &cache_path).await?;

    println!("Restored version #{index} of '{}' (recorded {}).",
        ctx.entry.remote_path, format_ts(v.recorded_at));
    println!("Marked Dirty — will upload on next sync window.");
    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

struct VersionCtx {
    db:       StateDb,
    entry:    FileEntry,
    mount:    MountConfig,
}

async fn resolve(config_path: &Path, user_path: &Path) -> Result<VersionCtx> {
    let cfg = crate::config_io::load(config_path)?;
    let user_abs = expand_tilde(user_path);

    let (mount, rel_path) = cfg.mounts.iter()
        .filter(|m| m.enabled)
        .filter_map(|m| {
            let mount_abs = expand_tilde(&m.resolved_mount_path());
            user_abs.strip_prefix(&mount_abs).ok().map(|rel| (m.clone(), rel.to_path_buf()))
        })
        .next()
        .ok_or_else(|| anyhow::anyhow!(
            "path {} is not under any configured mount", user_abs.display()
        ))?;

    let db_path = default_data_dir().join(format!("{}.db", mount.name));
    anyhow::ensure!(db_path.exists(), "no database for mount '{}'", mount.name);

    let db = StateDb::open(&db_path)?;
    let mount_id = db.get_mount_id(&mount.name).await?
        .ok_or_else(|| anyhow::anyhow!("mount '{}' not found in database", mount.name))?;

    let rel_str = rel_path.to_string_lossy();
    let rel_str = rel_str.trim_start_matches('/');

    let entry = if rel_str.is_empty() {
        db.get_by_remote_path(mount_id, "/").await?
    } else {
        let with_slash = format!("/{rel_str}");
        match db.get_by_remote_path(mount_id, &with_slash).await? {
            Some(e) => Some(e),
            None    => db.get_by_remote_path(mount_id, rel_str).await?,
        }
    };
    let entry = entry.ok_or_else(|| anyhow::anyhow!(
        "file not found in database: {rel_str}"
    ))?;

    Ok(VersionCtx { db, entry, mount })
}

/// Put the restored bytes at `cache_path` and mark the row dirty so the
/// daemon uploads them. The CLI can't reach the daemon's upload queue;
/// the daemon's cache-dir watcher is the trigger, and it only queues
/// rows that are already `dirty` when the event arrives. So: copy into
/// a dot-prefixed temp sibling (the watcher ignores dotfiles), mark the
/// row dirty, THEN rename into place — the rename event is guaranteed
/// to see `dirty`.
async fn install_restored_content(
    db: &StateDb, inode: Inode, blob: &Path, cache_path: &Path,
) -> Result<u64> {
    let name = cache_path.file_name()
        .ok_or_else(|| anyhow::anyhow!("cache path has no file name: {cache_path:?}"))?;
    let tmp = cache_path.with_file_name(
        format!(".{}.restore.tmp", name.to_string_lossy()));
    let written = std::fs::copy(blob, &tmp)
        .with_context(|| format!("copy blob {blob:?} -> {tmp:?}"))?;

    // Mark Dirty AND record the cache_path. When the entry was never
    // hydrated, `pick_or_synthesize_cache_path` returned a synthesized
    // path that isn't yet on the DB row — `set_status(Dirty)` alone
    // would leave `cache_path = NULL`, the same poisoned shape that
    // setattr/truncate used to produce. The migration-0010 trigger
    // would now reject that UPDATE outright; using
    // `set_dirty_with_cache_path` records both fields atomically.
    if let Err(e) = db.set_dirty_with_cache_path(inode, cache_path, written).await {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).context("marking restored entry Dirty");
    }
    std::fs::rename(&tmp, cache_path)
        .with_context(|| format!("rename {tmp:?} -> {cache_path:?}"))?;
    Ok(written)
}

fn pick_or_synthesize_cache_path(entry: &FileEntry, cache_dir: &Path) -> PathBuf {
    if let Some(cp) = &entry.cache_path { return cp.clone(); }
    // Reconstruct from remote_path. Drop the leading '/' if present.
    let rel = entry.remote_path.trim_start_matches('/');
    cache_dir.join(rel)
}

fn expand_tilde(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    p.to_owned()
}

fn format_ts(unix: i64) -> String {
    Local.timestamp_opt(unix, 0).single()
        .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|| unix.to_string())
}

fn short_hash(h: &str) -> &str {
    &h[..h.len().min(12)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;
    use stratosync_core::state::NewFileEntry;
    use stratosync_core::types::{FileKind, SyncStatus};

    /// The daemon's cache watcher only queues uploads for rows that are
    /// already `dirty` when the event is processed, so the restored
    /// bytes must land via a dot-prefixed temp file (ignored by the
    /// watcher) that is renamed into place only AFTER the row is dirty.
    #[tokio::test]
    async fn install_restored_content_marks_dirty_and_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let db = StateDb::in_memory().unwrap();
        db.migrate().await.unwrap();
        let mid = db.upsert_mount("t", "mock:/", "/mnt/t",
            dir.path().to_str().unwrap(), 1 << 30, 60).await.unwrap();
        let root = db.insert_root(&NewFileEntry {
            mount_id: mid, parent: 0, name: "/".into(), remote_path: "/".into(),
            kind: FileKind::Directory, size: 0, mtime: SystemTime::UNIX_EPOCH,
            etag: None, status: SyncStatus::Remote, cache_path: None, cache_size: None,
        }).await.unwrap();
        let cache_path = dir.path().join("docs/note.md");
        std::fs::create_dir_all(cache_path.parent().unwrap()).unwrap();
        std::fs::write(&cache_path, b"current").unwrap();
        let inode = db.insert_file(&NewFileEntry {
            mount_id: mid, parent: root, name: "note.md".into(),
            remote_path: "docs/note.md".into(), kind: FileKind::File, size: 7,
            mtime: SystemTime::UNIX_EPOCH, etag: Some("e".into()),
            status: SyncStatus::Cached, cache_path: Some(cache_path.clone()),
            cache_size: Some(7),
        }).await.unwrap();
        let blob = dir.path().join("blob");
        std::fs::write(&blob, b"older version").unwrap();

        let written = install_restored_content(&db, inode, &blob, &cache_path).await.unwrap();

        assert_eq!(written, 13);
        assert_eq!(std::fs::read(&cache_path).unwrap(), b"older version");
        let e = db.get_by_inode(inode).await.unwrap().unwrap();
        assert_eq!(e.status, SyncStatus::Dirty);
        assert_eq!(e.cache_path, Some(cache_path.clone()));
        let leftovers: Vec<_> = std::fs::read_dir(cache_path.parent().unwrap()).unwrap()
            .map(|d| d.unwrap().file_name()).collect();
        assert_eq!(leftovers.len(), 1, "temp file must be renamed away: {leftovers:?}");
    }
}
