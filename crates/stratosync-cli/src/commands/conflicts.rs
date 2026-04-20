use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use stratosync_core::{
    backend::RcloneBackend,
    base_store::BaseStore,
    config::{default_data_dir, MountConfig},
    merge::{self, MergeOutcome},
    state::StateDb,
    types::FileEntry,
    Backend,
};

// ── List conflicts (default subcommand) ──────────────────────────────────────

pub async fn list(config_path: &Path) -> Result<()> {
    let cfg = crate::config_io::load(config_path)?;
    let mut found_any = false;

    for mount in cfg.mounts.iter().filter(|m| m.enabled) {
        let db_path = default_data_dir().join(format!("{}.db", mount.name));

        if !db_path.exists() { continue; }

        let db = StateDb::open(&db_path)?;
        let Some(mount_id) = db.get_mount_id(&mount.name).await? else { continue };

        // Query for all conflict entries
        let conn = db.raw_conn().await;
        let mut stmt = conn.prepare(
            "SELECT inode, name, remote_path, size, mtime
             FROM file_index
             WHERE mount_id=?1 AND status='conflict'
             ORDER BY mtime DESC",
        )?;

        let rows: Vec<(u64, String, String, u64, i64)> = stmt
            .query_map(rusqlite::params![mount_id], |r| {
                Ok((
                    r.get::<_, i64>(0)? as u64,
                    r.get(1)?,
                    r.get(2)?,
                    r.get::<_, i64>(3)? as u64,
                    r.get(4)?,
                ))
            })?
            .filter_map(|r| r.ok())
            .collect();

        // Also look for files whose name contains ".conflict."
        let mut stmt2 = conn.prepare(
            "SELECT inode, name, remote_path, size, mtime
             FROM file_index
             WHERE mount_id=?1 AND name LIKE '%.conflict.%'
             ORDER BY mtime DESC",
        )?;
        let conflict_files: Vec<(u64, String, String, u64, i64)> = stmt2
            .query_map(rusqlite::params![mount_id], |r| {
                Ok((
                    r.get::<_, i64>(0)? as u64,
                    r.get(1)?,
                    r.get(2)?,
                    r.get::<_, i64>(3)? as u64,
                    r.get(4)?,
                ))
            })?
            .filter_map(|r| r.ok())
            .collect();

        if rows.is_empty() && conflict_files.is_empty() { continue; }

        found_any = true;
        println!("Mount: {}", mount.name);
        println!("{}", "─".repeat(60));

        for (inode, name, path, size, mtime) in &rows {
            let ts = chrono::DateTime::from_timestamp(*mtime, 0)
                .map(|d| d.format("%Y-%m-%d %H:%M UTC").to_string())
                .unwrap_or_else(|| "unknown".into());
            println!("  CONFLICT  {name}");
            println!("            inode={inode}  size={}  modified={ts}", bytesize::ByteSize(*size));
            println!("            remote: {path}");
            println!();
        }

        for (inode, name, path, size, mtime) in &conflict_files {
            let ts = chrono::DateTime::from_timestamp(*mtime, 0)
                .map(|d| d.format("%Y-%m-%d %H:%M UTC").to_string())
                .unwrap_or_else(|| "unknown".into());
            println!("  FILE      {name}");
            println!("            inode={inode}  size={}  modified={ts}", bytesize::ByteSize(*size));
            println!("            remote: {path}");
            println!();
        }
    }

    if !found_any {
        println!("No conflicts found.");
    } else {
        println!("To resolve:");
        println!("  stratosync conflicts keep-local  <path>   — upload local, discard remote");
        println!("  stratosync conflicts keep-remote <path>   — download remote, discard local");
        println!("  stratosync conflicts merge       <path>   — attempt 3-way merge");
        println!("  stratosync conflicts diff        <path>   — show differences");
        println!("  stratosync conflicts cleanup              — remove conflicts whose content matches the canonical");
    }

    Ok(())
}

// ── Path resolution ──────────────────────────────────────────────────────────

struct ResolveContext {
    db:               StateDb,
    mount_id:         u32,
    entry:            FileEntry,
    conflict_sibling: Option<FileEntry>,
    backend:          RcloneBackend,
    mount:            MountConfig,
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

/// Resolve a user-provided path to the mount, DB entry, backend, and any
/// conflict sibling file.
///
/// The path can be:
/// - A mount-relative path to a file with status='conflict'
/// - A path to a `.conflict.*` sibling file (resolves to the canonical entry)
async fn resolve_path(config_path: &Path, user_path: &Path) -> Result<ResolveContext> {
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

    // Build the remote path from the relative path — try both with and without
    // leading slash since root-level files may be stored either way.
    let rel_str = rel_path.to_string_lossy();
    let rel_str = rel_str.trim_start_matches('/');
    let entry = {
        let with_slash = format!("/{rel_str}");
        match db.get_by_remote_path(mount_id, &with_slash).await? {
            Some(e) => e,
            None => db.get_by_remote_path(mount_id, rel_str).await?
                .ok_or_else(|| anyhow::anyhow!(
                    "file not found in database: {rel_str}"
                ))?,
        }
    };

    // Find conflict sibling: look for .conflict.* files with the same stem
    let conflict_sibling = find_conflict_sibling(&db, mount_id, &entry).await?;

    let backend = RcloneBackend::new(&mount.remote)?;

    Ok(ResolveContext { db, mount_id, entry, conflict_sibling, backend, mount })
}

/// Find the `.conflict.*` sibling for a given entry, or if the entry itself
/// is a conflict sibling, find the canonical entry.
async fn find_conflict_sibling(
    db: &StateDb,
    mount_id: u32,
    entry: &FileEntry,
) -> Result<Option<FileEntry>> {
    let conn = db.raw_conn().await;

    if entry.name.contains(".conflict.") {
        // This IS the conflict sibling — find the canonical entry.
        // Strip .conflict.{ts}.{hash} from the name to get the canonical name.
        if let Some(idx) = entry.name.find(".conflict.") {
            let stem = &entry.name[..idx];
            let ext = entry.name.rsplit('.').next().unwrap_or("");
            let canonical_name = if ext.is_empty() || stem.ends_with(&format!(".{ext}")) {
                stem.to_owned()
            } else {
                format!("{stem}.{ext}")
            };
            // Look up canonical entry
            let rows: Vec<(i64,)> = conn.prepare(
                "SELECT inode FROM file_index
                 WHERE mount_id=?1 AND parent_inode=?2 AND name=?3"
            )?.query_map(
                rusqlite::params![mount_id, entry.parent, canonical_name],
                |r| Ok((r.get(0)?,))
            )?.filter_map(|r| r.ok()).collect();

            if let Some((inode,)) = rows.first() {
                return db.get_by_inode(*inode as u64).await;
            }
        }
        return Ok(None);
    }

    // Entry is canonical — find sibling with `.conflict.` in name
    let parent = entry.parent;
    let stem = Path::new(&entry.name).file_stem()
        .and_then(|s| s.to_str()).unwrap_or(&entry.name);

    let rows: Vec<(i64, String)> = conn.prepare(
        "SELECT inode, name FROM file_index
         WHERE mount_id=?1 AND parent_inode=?2 AND name LIKE ?3
         ORDER BY mtime DESC LIMIT 1"
    )?.query_map(
        rusqlite::params![mount_id, parent, format!("{stem}.conflict.%")],
        |r| Ok((r.get(0)?, r.get(1)?))
    )?.filter_map(|r| r.ok()).collect();

    if let Some((inode, _name)) = rows.first() {
        return db.get_by_inode(*inode as u64).await;
    }

    Ok(None)
}

/// Cleanup after resolution: delete conflict sibling from remote and DB,
/// then update canonical entry to Cached status.
async fn finalize_resolution(
    ctx: &ResolveContext,
    cache_path: &Path,
) -> Result<()> {
    // Delete conflict sibling
    if let Some(ref sibling) = ctx.conflict_sibling {
        if let Err(reason) = ctx.backend.delete(&sibling.remote_path).await {
            // NotFound is fine — sibling may have already been cleaned up
            if !matches!(reason, stratosync_core::types::SyncError::NotFound { .. }) {
                anyhow::bail!("failed to delete conflict file '{}': {reason}", sibling.remote_path);
            }
        }
        ctx.db.delete_entry(sibling.inode).await
            .context("failed to remove conflict sibling from database")?;
    }

    // Update canonical entry to Cached
    let meta = ctx.backend.stat(&ctx.entry.remote_path).await
        .context("failed to stat remote after resolution")?;
    ctx.db.set_cached(
        ctx.entry.inode, cache_path, meta.size,
        meta.etag.as_deref(), meta.mtime, meta.size,
    ).await.context("failed to update database status")?;

    Ok(())
}

// ── keep-local ───────────────────────────────────────────────────────────────

pub async fn keep_local(config_path: &Path, path: &Path) -> Result<()> {
    let ctx = resolve_path(config_path, path).await?;

    let cache_path = ctx.entry.cache_path.as_ref()
        .ok_or_else(|| anyhow::anyhow!(
            "'{}' is not cached locally — use keep-remote instead", ctx.entry.name
        ))?;

    anyhow::ensure!(cache_path.exists(),
        "local cache file missing: {}", cache_path.display());

    // Upload local version (no ETag check — force overwrite)
    ctx.backend.upload(cache_path, &ctx.entry.remote_path, None).await
        .context("failed to upload local version")?;

    finalize_resolution(&ctx, cache_path).await?;

    println!("Resolved: kept local version of '{}'", ctx.entry.name);
    Ok(())
}

// ── keep-remote ──────────────────────────────────────────────────────────────

pub async fn keep_remote(config_path: &Path, path: &Path) -> Result<()> {
    let ctx = resolve_path(config_path, path).await?;

    let cache_path = ctx.entry.cache_path.clone().unwrap_or_else(|| {
        ctx.mount.cache_dir().join(ctx.entry.remote_path.trim_start_matches('/'))
    });

    // Ensure parent directory exists
    if let Some(parent) = cache_path.parent() {
        std::fs::create_dir_all(parent)
            .context("failed to create cache directory")?;
    }

    // Download remote version to local cache
    ctx.backend.download(&ctx.entry.remote_path, &cache_path).await
        .context("failed to download remote version")?;

    finalize_resolution(&ctx, &cache_path).await?;

    println!("Resolved: kept remote version of '{}'", ctx.entry.name);
    Ok(())
}

// ── merge ────────────────────────────────────────────────────────────────────

pub async fn merge(config_path: &Path, path: &Path) -> Result<()> {
    let ctx = resolve_path(config_path, path).await?;

    let cache_path = ctx.entry.cache_path.as_ref()
        .ok_or_else(|| anyhow::anyhow!(
            "'{}' is not cached locally — use keep-remote instead", ctx.entry.name
        ))?;

    anyhow::ensure!(merge::git_available(),
        "git is required for merge but was not found in PATH");

    // Get base version
    let base_store = BaseStore::new(ctx.mount.cache_dir().join(".bases"))
        .context("failed to open base version store")?;

    let base_hash = ctx.db.get_base_hash(ctx.entry.inode, ctx.mount_id).await?
        .ok_or_else(|| anyhow::anyhow!(
            "no base version available for '{}' — use keep-local or keep-remote instead",
            ctx.entry.name
        ))?;

    let base_path = base_store.object_path(&base_hash);
    anyhow::ensure!(base_path.exists(),
        "base version file missing (hash: {base_hash}) — use keep-local or keep-remote instead");

    // Download remote to temp file
    let remote_tmp = cache_path.with_extension("stratosync-merge-remote");
    ctx.backend.download(&ctx.entry.remote_path, &remote_tmp).await
        .context("failed to download remote version for merge")?;

    match merge::try_three_way_merge(&base_path, cache_path, &remote_tmp) {
        MergeOutcome::Clean(merged) => {
            std::fs::write(cache_path, &merged)
                .context("failed to write merged content")?;
            std::fs::remove_file(&remote_tmp).ok();

            // Upload merged result
            ctx.backend.upload(cache_path, &ctx.entry.remote_path, None).await
                .context("failed to upload merged result")?;

            finalize_resolution(&ctx, cache_path).await?;

            // Update base version to the merged result
            if let Ok(hash) = base_store.store_base(cache_path) {
                let _ = ctx.db.set_base_hash(ctx.entry.inode, ctx.mount_id, &hash, 0).await;
            }

            println!("Merge succeeded cleanly — conflict resolved.");
        }
        MergeOutcome::ConflictMarkers(merged) => {
            std::fs::write(cache_path, &merged)
                .context("failed to write merged content with markers")?;
            std::fs::remove_file(&remote_tmp).ok();

            println!("Merge has conflicts. Edit the file to resolve conflict markers:");
            println!("  {}", cache_path.display());
            println!();
            println!("Then run:");
            println!("  stratosync conflicts keep-local {}", path.display());
        }
        MergeOutcome::Failed(reason) => {
            std::fs::remove_file(&remote_tmp).ok();
            anyhow::bail!("merge failed: {reason}");
        }
    }

    Ok(())
}

// ── cleanup ──────────────────────────────────────────────────────────────────

/// Walk every existing conflict entry and remove those whose content is
/// identical to their canonical sibling. Genuinely-differing conflicts are
/// left alone for manual resolution via keep-local/keep-remote/merge.
pub async fn cleanup(config_path: &Path, dry_run: bool) -> Result<()> {
    let cfg = crate::config_io::load(config_path)?;
    let mut total_checked = 0usize;
    let mut total_removed = 0usize;
    let mut total_kept = 0usize;
    let mut total_skipped = 0usize;

    for mount in cfg.mounts.iter().filter(|m| m.enabled) {
        let db_path = default_data_dir().join(format!("{}.db", mount.name));
        if !db_path.exists() { continue; }

        let db = StateDb::open(&db_path)?;
        let Some(mount_id) = db.get_mount_id(&mount.name).await? else { continue };
        let backend_dyn: std::sync::Arc<dyn Backend> =
            std::sync::Arc::new(RcloneBackend::new(&mount.remote)?);

        // Collect all conflict entries: by status or by filename pattern.
        let entries = collect_conflict_entries(&db, mount_id).await?;
        if entries.is_empty() {
            continue;
        }

        println!("Mount: {} ({} conflict entries)", mount.name, entries.len());
        println!("{}", "─".repeat(60));

        let work_dir = mount.cache_dir().join(".meta").join("cleanup");
        std::fs::create_dir_all(&work_dir).ok();

        for sibling in entries {
            total_checked += 1;

            let Some(canonical) = find_conflict_sibling(&db, mount_id, &sibling).await? else {
                println!("  SKIP  {} (no canonical file found)", sibling.name);
                total_skipped += 1;
                continue;
            };

            let equal = match stratosync_core::content::remote_eq_remote(
                &canonical.remote_path, &sibling.remote_path, &backend_dyn, &work_dir,
            ).await {
                Ok(eq) => eq,
                Err(e) => {
                    println!("  SKIP  {} (comparison failed: {e})", sibling.name);
                    total_skipped += 1;
                    continue;
                }
            };

            if !equal {
                println!("  KEEP  {} (content differs — resolve manually)", sibling.name);
                total_kept += 1;
                continue;
            }

            if dry_run {
                println!("  [dry-run] would remove {}", sibling.name);
                total_removed += 1;
                continue;
            }

            // Spurious conflict — delete from remote + DB
            match backend_dyn.delete(&sibling.remote_path).await {
                Ok(()) | Err(stratosync_core::types::SyncError::NotFound(_)) => {}
                Err(e) => {
                    println!("  SKIP  {} (remote delete failed: {e})", sibling.name);
                    total_skipped += 1;
                    continue;
                }
            }
            if let Err(e) = db.delete_entry(sibling.inode).await {
                println!("  SKIP  {} (db delete failed: {e})", sibling.name);
                total_skipped += 1;
                continue;
            }
            println!("  REMOVED  {} (spurious conflict)", sibling.name);
            total_removed += 1;
        }
        println!();
    }

    println!("Summary: checked={total_checked} removed={total_removed} kept={total_kept} skipped={total_skipped}");
    if dry_run && total_removed > 0 {
        println!("Re-run without --dry-run to actually remove the spurious conflicts.");
    }
    Ok(())
}

/// Collect every entry that looks like a conflict file — either by
/// `status='conflict'` or by filename pattern `%.conflict.%`.
async fn collect_conflict_entries(db: &StateDb, mount_id: u32) -> Result<Vec<FileEntry>> {
    let conn = db.raw_conn().await;
    let rows: Vec<u64> = conn.prepare(
        "SELECT inode FROM file_index
         WHERE mount_id=?1 AND (status='conflict' OR name LIKE '%.conflict.%')
         ORDER BY mtime DESC",
    )?
    .query_map(rusqlite::params![mount_id], |r| Ok(r.get::<_, i64>(0)? as u64))?
    .filter_map(|r| r.ok())
    .collect();

    drop(conn);

    let mut out = Vec::with_capacity(rows.len());
    for inode in rows {
        if let Some(e) = db.get_by_inode(inode).await? {
            out.push(e);
        }
    }
    Ok(out)
}

// ── diff ─────────────────────────────────────────────────────────────────────

pub async fn diff(config_path: &Path, path: &Path) -> Result<()> {
    let ctx = resolve_path(config_path, path).await?;

    let cache_path = ctx.entry.cache_path.as_ref()
        .ok_or_else(|| anyhow::anyhow!(
            "'{}' is not cached locally — cannot diff", ctx.entry.name
        ))?;

    // Download remote to temp file
    let remote_tmp = cache_path.with_extension("stratosync-diff-remote");
    ctx.backend.download(&ctx.entry.remote_path, &remote_tmp).await
        .context("failed to download remote version for diff")?;

    let status = std::process::Command::new("diff")
        .args(["-u", "--label", "local", "--label", "remote"])
        .arg(cache_path)
        .arg(&remote_tmp)
        .status()
        .context("failed to run diff")?;

    std::fs::remove_file(&remote_tmp).ok();

    // diff exits 0=same, 1=different, 2=error
    if status.code() == Some(2) {
        anyhow::bail!("diff command failed");
    }

    Ok(())
}
