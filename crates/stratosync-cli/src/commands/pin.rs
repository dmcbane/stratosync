use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use stratosync_core::{
    backend::RcloneBackend,
    config::{default_data_dir, MountConfig},
    state::StateDb,
    types::{FileEntry, FileKind},
    Backend,
};

// ── Pin ──────────────────────────────────────────────────────────────────────

pub async fn pin(config_path: &Path, user_path: &Path) -> Result<()> {
    let ctx = resolve_path(config_path, user_path).await?;
    let mut count = 0u64;

    if ctx.entry.kind == FileKind::Directory {
        // Recursively pin all file descendants
        let descendants = ctx.db.list_file_descendants(ctx.mount_id, &ctx.entry.remote_path).await?;
        for child in &descendants {
            pin_single_file(&ctx.db, &ctx.backend, &ctx.mount, child).await?;
            count += 1;
        }
    } else {
        pin_single_file(&ctx.db, &ctx.backend, &ctx.mount, &ctx.entry).await?;
        count = 1;
    }

    if count == 1 {
        println!("Pinned '{}'", ctx.entry.name);
    } else {
        println!("Pinned {count} file(s) under '{}'", ctx.entry.name);
    }
    Ok(())
}

async fn pin_single_file(
    db: &StateDb,
    backend: &RcloneBackend,
    mount: &MountConfig,
    entry: &FileEntry,
) -> Result<()> {
    // If not hydrated, download the file so it's available offline
    if entry.status.needs_hydration() {
        let cache_path = mount.cache_dir().join(entry.remote_path.trim_start_matches('/'));
        if let Some(parent) = cache_path.parent() {
            std::fs::create_dir_all(parent)
                .context("failed to create cache directory")?;
        }
        backend.download(&entry.remote_path, &cache_path).await
            .map_err(|e| anyhow::anyhow!("failed to download '{}': {e}", entry.name))?;

        let meta = backend.stat(&entry.remote_path).await
            .map_err(|e| anyhow::anyhow!("failed to stat '{}': {e}", entry.name))?;
        db.set_cached(
            entry.inode, &cache_path, meta.size,
            meta.etag.as_deref(), meta.mtime, meta.size,
        ).await.context("failed to update cache status")?;
    }

    db.set_pinned(entry.inode, true).await
        .context("failed to set pin flag")?;
    Ok(())
}

// ── Unpin ────────────────────────────────────────────────────────────────────

pub async fn unpin(config_path: &Path, user_path: &Path) -> Result<()> {
    let ctx = resolve_path(config_path, user_path).await?;
    let mut count = 0u64;

    if ctx.entry.kind == FileKind::Directory {
        let descendants = ctx.db.list_file_descendants(ctx.mount_id, &ctx.entry.remote_path).await?;
        for child in &descendants {
            ctx.db.set_pinned(child.inode, false).await?;
            count += 1;
        }
    } else {
        ctx.db.set_pinned(ctx.entry.inode, false).await?;
        count = 1;
    }

    if count == 1 {
        println!("Unpinned '{}'", ctx.entry.name);
    } else {
        println!("Unpinned {count} file(s) under '{}'", ctx.entry.name);
    }
    Ok(())
}

// ── Path resolution (shared with conflicts.rs pattern) ───────────────────────

struct PinContext {
    db:       StateDb,
    mount_id: u32,
    entry:    FileEntry,
    backend:  RcloneBackend,
    mount:    MountConfig,
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

async fn resolve_path(config_path: &Path, user_path: &Path) -> Result<PinContext> {
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

    let remote_path = if rel_path.as_os_str().is_empty() {
        "/".to_owned()
    } else {
        format!("/{}", rel_path.to_string_lossy().trim_start_matches('/'))
    };

    let entry = db.get_by_remote_path(mount_id, &remote_path).await?
        .ok_or_else(|| anyhow::anyhow!("file not found in database: {remote_path}"))?;

    let backend = RcloneBackend::new(&mount.remote)?;

    Ok(PinContext { db, mount_id, entry, backend, mount })
}
