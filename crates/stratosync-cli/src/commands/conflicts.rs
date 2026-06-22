use std::path::{Path, PathBuf};
use std::sync::Arc;

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

        let entries = collect_conflict_entries(&db, mount_id).await?;
        if entries.is_empty() { continue; }

        found_any = true;
        let mount_fuse = expand_tilde(&mount.resolved_mount_path());
        println!("Mount: {}", mount.name);
        println!("{}", "─".repeat(60));

        for sibling in &entries {
            let ts = chrono::DateTime::from_timestamp(
                    sibling.mtime.duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default().as_secs() as i64, 0)
                .map(|d| d.format("%Y-%m-%d %H:%M UTC").to_string())
                .unwrap_or_else(|| "unknown".into());

            // Locate the canonical entry so we can show its FUSE path.
            let canonical_fuse = if let Ok(Some(canon)) =
                find_conflict_sibling(&db, mount_id, sibling).await
            {
                let rel = canon.remote_path.trim_start_matches('/');
                Some(mount_fuse.join(rel))
            } else {
                None
            };

            println!("  {}", sibling.name);
            println!("    conflict:  {}", sibling.remote_path);
            println!("    size: {}  |  modified: {ts}",
                bytesize::ByteSize(sibling.size));
            if let Some(ref canon_path) = canonical_fuse {
                println!("    resolve via: {}", canon_path.display());
            }
            println!();
        }
    }

    if !found_any {
        println!("No conflicts found.");
    } else {
        println!("To resolve, pass the canonical file path shown above:");
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
    /// Always the canonical entry (never the .conflict.* sibling).
    entry:            FileEntry,
    /// The .conflict.* sibling, when one exists.
    conflict_sibling: Option<FileEntry>,
    backend:          Arc<dyn Backend>,
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
/// - A mount-relative path to the canonical file (e.g. `/mount/docs/report.pdf`)
/// - A FUSE-visible path to the `.conflict.*` sibling (resolves via name lookup)
///
/// In either case `ctx.entry` is always the *canonical* entry and
/// `ctx.conflict_sibling` is the `.conflict.*` sibling (if found).
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

    let rel_str = rel_path.to_string_lossy();
    let rel_str = rel_str.trim_start_matches('/');

    // Primary lookup: by remote_path (exact match, with/without leading slash).
    let entry_raw = {
        let with_slash = format!("/{rel_str}");
        match db.get_by_remote_path(mount_id, &with_slash).await? {
            Some(e) => Some(e),
            None    => db.get_by_remote_path(mount_id, rel_str).await?,
        }
    };

    // Fallback for conflict sibling FUSE paths: conflict siblings are stored
    // under .stratosync-conflicts/ so their remote_path never matches the
    // user-visible FUSE path.  When the filename contains ".conflict." and the
    // primary lookup found nothing, search by name within the mount.
    let entry_raw = match entry_raw {
        Some(e) => e,
        None => {
            let fuse_name = Path::new(rel_str)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(rel_str);

            if fuse_name.contains(".conflict.") {
                let inode = {
                    let conn = db.raw_conn().await;
                    let inodes: Vec<u64> = conn.prepare(
                        "SELECT inode FROM file_index
                         WHERE mount_id=?1 AND name=?2
                         LIMIT 1",
                    )?.query_map(
                        rusqlite::params![mount_id, fuse_name],
                        |r| Ok(r.get::<_, i64>(0)? as u64),
                    )?.filter_map(|r| r.ok()).collect();
                    inodes.into_iter().next()
                };
                match inode {
                    Some(i) => db.get_by_inode(i).await?
                        .ok_or_else(|| anyhow::anyhow!("file not found in database: {rel_str}"))?,
                    None => anyhow::bail!("file not found in database: {rel_str}"),
                }
            } else {
                anyhow::bail!("file not found in database: {rel_str}")
            }
        }
    };

    // Normalize roles: ctx.entry must always be the canonical; if the lookup
    // returned a conflict sibling, swap so we hold (canonical, sibling).
    let (entry, conflict_sibling) =
        normalize_conflict_roles(&db, mount_id, entry_raw).await?;

    let backend: Arc<dyn Backend> = Arc::new(RcloneBackend::new(&mount.remote)?);

    Ok(ResolveContext { db, mount_id, entry, conflict_sibling, backend, mount })
}

/// Return `(canonical, sibling?)` regardless of which end of the pair `entry`
/// represents.  If `entry` already is the canonical, calls
/// `find_conflict_sibling` to locate its sibling.  If `entry` is the sibling
/// (contains ".conflict." in name), locates the canonical instead and swaps.
async fn normalize_conflict_roles(
    db: &StateDb,
    mount_id: u32,
    entry: FileEntry,
) -> Result<(FileEntry, Option<FileEntry>)> {
    if entry.name.contains(".conflict.") {
        // Entry is the sibling — find canonical via the reverse lookup.
        let canonical = find_conflict_sibling(db, mount_id, &entry)
            .await?
            .ok_or_else(|| anyhow::anyhow!(
                "'{}' looks like a conflict file but its canonical entry could not \
                 be found in the database",
                entry.name
            ))?;
        Ok((canonical, Some(entry)))
    } else {
        // Entry is the canonical — find sibling if present.
        let sibling = find_conflict_sibling(db, mount_id, &entry).await?;
        Ok((entry, sibling))
    }
}

/// Find the `.conflict.*` sibling for a given entry, or if the entry itself
/// is a conflict sibling, find the canonical entry.
async fn find_conflict_sibling(
    db: &StateDb,
    mount_id: u32,
    entry: &FileEntry,
) -> Result<Option<FileEntry>> {
    // IMPORTANT: release the raw_conn MutexGuard BEFORE calling any other
    // StateDb async method — those methods also lock `conn`, and holding
    // the guard across an await on them would self-deadlock.
    let target_inode: Option<u64> = {
        let conn = db.raw_conn().await;

        if entry.name.contains(".conflict.") {
            if let Some(idx) = entry.name.find(".conflict.") {
                let stem = &entry.name[..idx];
                let ext = entry.name.rsplit('.').next().unwrap_or("");
                let canonical_name = if ext.is_empty() || stem.ends_with(&format!(".{ext}")) {
                    stem.to_owned()
                } else {
                    format!("{stem}.{ext}")
                };
                let rows: Vec<i64> = conn.prepare(
                    "SELECT inode FROM file_index
                     WHERE mount_id=?1 AND parent_inode=?2 AND name=?3"
                )?.query_map(
                    rusqlite::params![mount_id, entry.parent, canonical_name],
                    |r| r.get(0),
                )?.filter_map(|r| r.ok()).collect();
                rows.first().map(|&i| i as u64)
            } else {
                None
            }
        } else {
            // Entry is canonical — find sibling with `.conflict.` in name.
            let stem = Path::new(&entry.name).file_stem()
                .and_then(|s| s.to_str()).unwrap_or(&entry.name);
            let rows: Vec<i64> = conn.prepare(
                "SELECT inode FROM file_index
                 WHERE mount_id=?1 AND parent_inode=?2 AND name LIKE ?3
                 ORDER BY mtime DESC LIMIT 1"
            )?.query_map(
                rusqlite::params![mount_id, entry.parent, format!("{stem}.conflict.%")],
                |r| r.get(0),
            )?.filter_map(|r| r.ok()).collect();
            rows.first().map(|&i| i as u64)
        }
        // `conn` dropped here — lock released.
    };

    match target_inode {
        Some(inode) => db.get_by_inode(inode).await,
        None        => Ok(None),
    }
}

/// Cleanup after resolution: delete conflict sibling from remote and DB,
/// then update canonical entry to Cached status.
async fn finalize_resolution(
    db: &StateDb,
    backend: &dyn Backend,
    entry: &FileEntry,
    conflict_sibling: Option<&FileEntry>,
    cache_path: &Path,
) -> Result<()> {
    // Delete conflict sibling
    if let Some(sibling) = conflict_sibling {
        if let Err(reason) = backend.delete(&sibling.remote_path).await {
            // NotFound is fine — sibling may have already been cleaned up
            if !matches!(reason, stratosync_core::types::SyncError::NotFound { .. }) {
                anyhow::bail!("failed to delete conflict file '{}': {reason}", sibling.remote_path);
            }
        }
        db.delete_entry(sibling.inode).await
            .context("failed to remove conflict sibling from database")?;
    }

    // Update canonical entry to Cached
    let meta = backend.stat(&entry.remote_path).await
        .context("failed to stat remote after resolution")?;
    db.set_cached(
        entry.inode, cache_path, meta.size,
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

    finalize_resolution(
        &ctx.db, &*ctx.backend,
        &ctx.entry, ctx.conflict_sibling.as_ref(),
        cache_path,
    ).await?;

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

    finalize_resolution(
        &ctx.db, &*ctx.backend,
        &ctx.entry, ctx.conflict_sibling.as_ref(),
        &cache_path,
    ).await?;

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

            finalize_resolution(
                &ctx.db, &*ctx.backend,
                &ctx.entry, ctx.conflict_sibling.as_ref(),
                cache_path,
            ).await?;

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
        let backend_dyn: Arc<dyn Backend> =
            Arc::new(RcloneBackend::new(&mount.remote)?);

        // Collect all conflict entries: by status or by filename pattern.
        let entries = collect_conflict_entries(&db, mount_id).await?;
        if entries.is_empty() {
            continue;
        }

        let total = entries.len();
        println!("Mount: {} ({total} conflict entries)", mount.name);
        println!("{}", "─".repeat(60));

        let work_dir = mount.cache_dir().join(".meta").join("cleanup");
        std::fs::create_dir_all(&work_dir).ok();

        for (i, sibling) in entries.into_iter().enumerate() {
            total_checked += 1;
            let idx = i + 1;

            let line_prefix = format!("  [{idx:>3}/{total}] {}", sibling.name);
            print_progress(&line_prefix, "…");

            let Some(canonical) = find_conflict_sibling(&db, mount_id, &sibling).await? else {
                finish_progress(&line_prefix, "SKIP (no canonical in DB)");
                total_skipped += 1;
                continue;
            };

            // Fast path: stat both sides and compare content hashes. For
            // Google Drive / OneDrive etc. this is decisive with zero
            // downloads. Falls back to a byte-level comparison only when
            // the provider doesn't expose a content hash for one side.
            let equal = match run_with_spinner(&line_prefix, "stat", async {
                stratosync_core::content::stat_remote_eq(
                    &canonical.remote_path, &sibling.remote_path, &backend_dyn,
                ).await
            }).await {
                Ok(stratosync_core::content::StatEqResult::Equal) => true,
                Ok(stratosync_core::content::StatEqResult::Different) => false,
                Ok(stratosync_core::content::StatEqResult::Unknown) => {
                    // Fall back to byte comparison.
                    let fallback = match canonical.cache_path.as_ref() {
                        Some(cp) if cp.exists() => run_with_spinner(&line_prefix, "verify", async {
                            stratosync_core::content::local_eq_remote(
                                cp, &sibling.remote_path, &backend_dyn,
                            ).await.map(|(eq, _)| eq)
                        }).await,
                        _ => run_with_spinner(&line_prefix, "download", async {
                            stratosync_core::content::remote_eq_remote(
                                &canonical.remote_path, &sibling.remote_path,
                                &backend_dyn, &work_dir,
                            ).await
                        }).await,
                    };
                    match fallback {
                        Ok(eq) => eq,
                        Err(e) => {
                            finish_progress(&line_prefix,
                                &format!("SKIP (comparison failed: {e})"));
                            total_skipped += 1;
                            continue;
                        }
                    }
                }
                Err(e) => {
                    // Most common case: the conflict sibling's remote file
                    // no longer exists (deleted out-of-band, or never made it
                    // during an earlier aborted resolution). The DB still
                    // tracks it, so clean up the orphan row.
                    let err_msg = format!("{e}");
                    let is_missing = err_msg.contains("not found")
                        || err_msg.contains("directory not found");
                    if is_missing {
                        if dry_run {
                            finish_progress(&line_prefix,
                                "WOULD REMOVE (orphan: remote gone)");
                            total_removed += 1;
                        } else {
                            match db.delete_entry(sibling.inode).await {
                                Ok(_) => {
                                    finish_progress(&line_prefix,
                                        "REMOVED (orphan: remote gone)");
                                    total_removed += 1;
                                }
                                Err(de) => {
                                    finish_progress(&line_prefix,
                                        &format!("SKIP (db delete failed: {de})"));
                                    total_skipped += 1;
                                }
                            }
                        }
                    } else {
                        finish_progress(&line_prefix,
                            &format!("SKIP (stat failed: {e})"));
                        total_skipped += 1;
                    }
                    continue;
                }
            };

            if !equal {
                finish_progress(&line_prefix, "KEEP (content differs)");
                total_kept += 1;
                continue;
            }

            if dry_run {
                finish_progress(&line_prefix, "WOULD REMOVE");
                total_removed += 1;
                continue;
            }

            // Spurious conflict — delete from remote + DB
            match backend_dyn.delete(&sibling.remote_path).await {
                Ok(()) | Err(stratosync_core::types::SyncError::NotFound(_)) => {}
                Err(e) => {
                    finish_progress(&line_prefix,
                        &format!("SKIP (remote delete failed: {e})"));
                    total_skipped += 1;
                    continue;
                }
            }
            if let Err(e) = db.delete_entry(sibling.inode).await {
                finish_progress(&line_prefix,
                    &format!("SKIP (db delete failed: {e})"));
                total_skipped += 1;
                continue;
            }
            finish_progress(&line_prefix, "REMOVED");
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

/// Write a single-line progress indicator: `<prefix> ... <state>` with a
/// carriage return so it can be overwritten by later calls.
fn print_progress(prefix: &str, state: &str) {
    use std::io::Write;
    // Trailing spaces clear any longer previous line.
    print!("\r{prefix} ... {state}    ");
    let _ = std::io::stdout().flush();
}

/// Finalize the progress line with a terminal newline so the next line
/// starts fresh. Pads with spaces to wipe any previous state text.
fn finish_progress(prefix: &str, result: &str) {
    println!("\r{prefix} ... {result}                                        ");
}

/// Run a future while updating the progress line with elapsed seconds.
/// Spawns a background ticker that prints `(elapsed Ns)` every second so
/// long rclone calls look alive.
async fn run_with_spinner<F, T>(prefix: &str, stage: &str, fut: F) -> T
where
    F: std::future::Future<Output = T>,
{
    let start = std::time::Instant::now();
    let prefix_owned = prefix.to_string();
    let stage_owned = stage.to_string();
    let ticker = tokio::spawn(async move {
        // First tick: show stage immediately, then update every second.
        print_progress(&prefix_owned, &stage_owned);
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            let elapsed = start.elapsed().as_secs();
            print_progress(&prefix_owned, &format!("{stage_owned} ({elapsed}s)"));
        }
    });
    let out = fut.await;
    ticker.abort();
    out
}

/// Collect every entry that looks like a conflict file — either by
/// `status='conflict'` or by filename pattern `%.conflict.%`.
/// Results are deduplicated by inode so a sibling that matches both
/// conditions appears only once.
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

    // Deduplicate: a sibling satisfies both conditions so appears once in the
    // SQL result but could map to the same inode twice if the query is ever
    // changed to use UNION.  Using a seen-set is defensive.
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(rows.len());
    for inode in rows {
        if seen.insert(inode) {
            if let Some(e) = db.get_by_inode(inode).await? {
                out.push(e);
            }
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

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::SystemTime;
    use stratosync_core::{
        backend::mock::MockBackend,
        state::{NewFileEntry, StateDb},
        types::{FileKind, SyncStatus, FUSE_ROOT_INODE},
    };

    async fn setup_db() -> (StateDb, u32) {
        let db = StateDb::in_memory().unwrap();
        db.migrate().await.unwrap();
        let mount_id = db.upsert_mount(
            "test", "mock:/", "/mnt/test",
            "/tmp/stratosync-test-cache", 5 << 30, 60,
        ).await.unwrap();
        db.insert_root(&NewFileEntry {
            mount_id, parent: 0,
            name: "/".into(), remote_path: "/".into(),
            kind: FileKind::Directory, size: 0,
            mtime: SystemTime::UNIX_EPOCH, etag: None,
            status: SyncStatus::Remote,
            cache_path: None, cache_size: None,
        }).await.unwrap();
        (db, mount_id)
    }

    async fn insert_canonical(
        db: &StateDb,
        mount_id: u32,
        name: &str,
        cache_path: Option<PathBuf>,
    ) -> u64 {
        // The DB trigger enforces: status=Cached requires cache_path.
        // Use Remote when no local file is provided.
        let (status, cache_size) = if cache_path.is_some() {
            (SyncStatus::Cached, Some(100))
        } else {
            (SyncStatus::Remote, None)
        };
        db.insert_file(&NewFileEntry {
            mount_id,
            parent: FUSE_ROOT_INODE,
            name: name.into(),
            remote_path: format!("/{name}"),
            kind: FileKind::File,
            size: 100,
            mtime: SystemTime::now(),
            etag: Some("etag-canonical".into()),
            status,
            cache_path,
            cache_size,
        }).await.unwrap()
    }

    async fn insert_sibling(db: &StateDb, mount_id: u32, sibling_name: &str) -> u64 {
        db.insert_file(&NewFileEntry {
            mount_id,
            parent: FUSE_ROOT_INODE,
            name: sibling_name.into(),
            remote_path: format!(".stratosync-conflicts/{sibling_name}"),
            kind: FileKind::File,
            size: 80,
            mtime: SystemTime::now(),
            etag: None,
            status: SyncStatus::Conflict,
            cache_path: None,
            cache_size: None,
        }).await.unwrap()
    }

    // ── find_conflict_sibling ────────────────────────────────────────────────

    #[tokio::test]
    async fn find_sibling_from_canonical() {
        let (db, mount_id) = setup_db().await;
        let canonical_inode = insert_canonical(&db, mount_id, "report.pdf", None).await;
        let sibling_inode =
            insert_sibling(&db, mount_id, "report.conflict.20250101T000000Z.deadbeef.pdf").await;

        let canonical = db.get_by_inode(canonical_inode).await.unwrap().unwrap();
        let found = find_conflict_sibling(&db, mount_id, &canonical).await.unwrap();

        assert!(found.is_some(), "should find conflict sibling from canonical");
        assert_eq!(found.unwrap().inode, sibling_inode);
    }

    #[tokio::test]
    async fn find_canonical_from_sibling() {
        let (db, mount_id) = setup_db().await;
        let canonical_inode = insert_canonical(&db, mount_id, "report.pdf", None).await;
        let sibling_inode =
            insert_sibling(&db, mount_id, "report.conflict.20250101T000000Z.deadbeef.pdf").await;

        let sibling = db.get_by_inode(sibling_inode).await.unwrap().unwrap();
        let found = find_conflict_sibling(&db, mount_id, &sibling).await.unwrap();

        assert!(found.is_some(), "should find canonical from sibling");
        assert_eq!(found.unwrap().inode, canonical_inode);
    }

    // ── normalize_conflict_roles ─────────────────────────────────────────────

    #[tokio::test]
    async fn normalize_from_canonical_returns_canonical_plus_sibling() {
        let (db, mount_id) = setup_db().await;
        let canonical_inode = insert_canonical(&db, mount_id, "doc.txt", None).await;
        let sibling_inode =
            insert_sibling(&db, mount_id, "doc.conflict.20250101T000000Z.deadbeef.txt").await;

        let canonical = db.get_by_inode(canonical_inode).await.unwrap().unwrap();
        let (entry, sib) =
            normalize_conflict_roles(&db, mount_id, canonical).await.unwrap();

        assert_eq!(entry.inode, canonical_inode, "entry must be the canonical");
        assert_eq!(sib.as_ref().map(|s| s.inode), Some(sibling_inode));
    }

    #[tokio::test]
    async fn normalize_from_sibling_swaps_to_canonical() {
        let (db, mount_id) = setup_db().await;
        let canonical_inode = insert_canonical(&db, mount_id, "doc.txt", None).await;
        let sibling_inode =
            insert_sibling(&db, mount_id, "doc.conflict.20250101T000000Z.deadbeef.txt").await;

        let sibling = db.get_by_inode(sibling_inode).await.unwrap().unwrap();
        let (entry, sib) =
            normalize_conflict_roles(&db, mount_id, sibling).await.unwrap();

        assert_eq!(entry.inode, canonical_inode,
            "entry must be the canonical, not the sibling");
        assert_eq!(sib.as_ref().map(|s| s.inode), Some(sibling_inode));
    }

    // ── finalize_resolution ──────────────────────────────────────────────────

    #[tokio::test]
    async fn finalize_resolution_removes_sibling_and_caches_canonical() {
        let (db, mount_id) = setup_db().await;
        let dir = tempfile::tempdir().unwrap();
        let cache_file = dir.path().join("report.pdf");
        std::fs::write(&cache_file, b"remote content").unwrap();

        let canonical_inode =
            insert_canonical(&db, mount_id, "report.pdf", Some(cache_file.clone())).await;
        let sibling_inode =
            insert_sibling(&db, mount_id, "report.conflict.20250101T000000Z.deadbeef.pdf").await;

        let canonical = db.get_by_inode(canonical_inode).await.unwrap().unwrap();
        let sibling   = db.get_by_inode(sibling_inode).await.unwrap().unwrap();

        let backend = MockBackend::default();
        // Seed the mock: canonical must exist for stat() to succeed.
        let backend_arc: Arc<dyn Backend> = Arc::new(backend);
        backend_arc.upload(&cache_file, "/report.pdf", None).await.unwrap();
        // Seed sibling so delete() doesn't error.
        backend_arc.upload(
            &cache_file,
            ".stratosync-conflicts/report.conflict.20250101T000000Z.deadbeef.pdf",
            None,
        ).await.unwrap();

        finalize_resolution(
            &db, &*backend_arc,
            &canonical, Some(&sibling),
            &cache_file,
        ).await.unwrap();

        // Sibling must be gone from DB.
        let sib_after = db.get_by_inode(sibling_inode).await.unwrap();
        assert!(sib_after.is_none(), "sibling should be removed from DB after resolution");

        // Canonical must be Cached.
        let canon_after = db.get_by_inode(canonical_inode).await.unwrap().unwrap();
        assert_eq!(canon_after.status, SyncStatus::Cached,
            "canonical should be Cached after resolution");
    }

    /// Core regression: keep-remote called via the sibling's FUSE-visible name
    /// (e.g. /mount/docs/file.conflict.…txt) should remove the sibling from DB.
    /// Before the fix this would fail with "file not found in database" because
    /// the sibling's remote_path is under .stratosync-conflicts/ and never
    /// matched the FUSE path — the conflict stayed in the list forever.
    #[tokio::test]
    async fn finalize_after_normalize_from_sibling_clears_conflict() {
        let (db, mount_id) = setup_db().await;
        let dir = tempfile::tempdir().unwrap();
        let cache_file = dir.path().join("report.pdf");
        std::fs::write(&cache_file, b"remote content").unwrap();

        let canonical_inode =
            insert_canonical(&db, mount_id, "report.pdf", Some(cache_file.clone())).await;
        let sibling_inode =
            insert_sibling(&db, mount_id, "report.conflict.20250101T000000Z.deadbeef.pdf").await;

        // Simulate what happens when the user passes the sibling's FUSE path:
        // normalize_conflict_roles is called with the sibling entry and must
        // swap roles before finalize_resolution runs.
        let sibling_entry = db.get_by_inode(sibling_inode).await.unwrap().unwrap();
        let (canonical, conflict_sibling) =
            normalize_conflict_roles(&db, mount_id, sibling_entry).await.unwrap();

        assert_eq!(canonical.inode, canonical_inode,
            "normalize must produce the canonical as ctx.entry");
        assert_eq!(conflict_sibling.as_ref().map(|s| s.inode), Some(sibling_inode),
            "normalize must keep sibling in conflict_sibling slot");

        let backend_arc: Arc<dyn Backend> = Arc::new(MockBackend::default());
        backend_arc.upload(&cache_file, "/report.pdf", None).await.unwrap();
        backend_arc.upload(
            &cache_file,
            ".stratosync-conflicts/report.conflict.20250101T000000Z.deadbeef.pdf",
            None,
        ).await.unwrap();

        finalize_resolution(
            &db, &*backend_arc,
            &canonical, conflict_sibling.as_ref(),
            &cache_file,
        ).await.unwrap();

        // The conflict sibling must no longer appear in the DB.
        assert!(db.get_by_inode(sibling_inode).await.unwrap().is_none(),
            "sibling must be gone from DB — conflict no longer reported");

        // Canonical must be healthy.
        let canon = db.get_by_inode(canonical_inode).await.unwrap().unwrap();
        assert_eq!(canon.status, SyncStatus::Cached);
    }

    /// `collect_conflict_entries` must return each sibling exactly once even
    /// though it matches BOTH the status='conflict' and name LIKE conditions.
    #[tokio::test]
    async fn collect_conflict_entries_no_duplicates() {
        let (db, mount_id) = setup_db().await;
        insert_canonical(&db, mount_id, "file.txt", None).await;
        insert_sibling(&db, mount_id, "file.conflict.20250101T000000Z.deadbeef.txt").await;

        let entries = collect_conflict_entries(&db, mount_id).await.unwrap();
        assert_eq!(entries.len(), 1, "sibling should appear exactly once, got: {entries:?}");
    }
}
