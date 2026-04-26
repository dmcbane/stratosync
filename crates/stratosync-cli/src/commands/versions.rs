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
    types::{FileEntry, SyncStatus},
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
    std::fs::copy(&blob, &cache_path)
        .with_context(|| format!("copy blob {blob:?} -> {cache_path:?}"))?;

    // Mark Dirty so the next sync uploads it.
    ctx.db.set_status(ctx.entry.inode, SyncStatus::Dirty).await
        .context("marking restored entry Dirty")?;

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
