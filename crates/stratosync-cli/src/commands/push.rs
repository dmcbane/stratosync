//! `stratosync push <path>` — force-upload a file (or directory of
//! files) right now, bypassing the per-write debounce and bandwidth
//! window. The mirror of `pin`/`unpin` for the upload direction.
//!
//! Implementation strategy: open the file through the FUSE mount and
//! call `sync_all()`. The kernel routes that to the daemon's FUSE
//! `fsync` callback, which already enqueues `UploadTrigger::Fsync`
//! (window=ZERO, immediate=true) — so all the bypass machinery already
//! lives in the daemon and we don't need a new IPC route.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use stratosync_core::{
    config::{default_data_dir, MountConfig},
    state::StateDb,
    types::{FileEntry, FileKind, SyncStatus},
};

/// What `push` should do for a single file, derived from its current
/// `SyncStatus`. Pure function so the decision matrix is unit-testable
/// without spinning up FUSE or a real DB.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PushAction {
    /// File has local edits not yet on the remote — fsync it through
    /// the FUSE mount to trigger an immediate upload.
    Fsync,
    /// File is already in sync with the remote; nothing to push.
    /// Reported as success with a friendly message.
    AlreadySynced,
    /// File has no local copy yet (Remote/Hydrating/Stale) or is in
    /// conflict — pushing is either nonsensical or destructive. Caller
    /// surfaces the message as an error.
    Refuse(&'static str),
}

fn classify_for_push(status: SyncStatus) -> PushAction {
    match status {
        SyncStatus::Dirty | SyncStatus::Uploading => PushAction::Fsync,
        SyncStatus::Cached                        => PushAction::AlreadySynced,
        SyncStatus::Remote | SyncStatus::Hydrating => PushAction::Refuse(
            "file is not downloaded locally — nothing to push"),
        SyncStatus::Stale => PushAction::Refuse(
            "file is stale (remote has newer content) — re-open to re-hydrate, then edit and push"),
        SyncStatus::Conflict => PushAction::Refuse(
            "file is in conflict state — resolve with `stratosync conflicts` first"),
    }
}

pub async fn push(config_path: &Path, user_path: &Path) -> Result<()> {
    let ctx = resolve_path(config_path, user_path).await?;

    if ctx.entry.kind == FileKind::Directory {
        // Mirror `pin`: recurse and push every dirty file underneath.
        // Files that are already synced are silently skipped — the
        // caller asked us to flush a directory, not to be pedantic
        // about each leaf.
        let descendants = ctx.db.list_file_descendants(ctx.mount_id, &ctx.entry.remote_path).await?;
        let mut pushed = 0u64;
        let mut skipped = 0u64;
        let mut refused: Vec<(String, &'static str)> = Vec::new();

        for child in &descendants {
            match classify_for_push(child.status) {
                PushAction::Fsync => {
                    fsync_through_mount(&ctx.mount, child)?;
                    pushed += 1;
                }
                PushAction::AlreadySynced => skipped += 1,
                PushAction::Refuse(reason) => {
                    refused.push((child.remote_path.clone(), reason));
                }
            }
        }

        if pushed == 0 && refused.is_empty() {
            println!("Nothing to push under '{}' ({} file(s) already synced)",
                     ctx.entry.name, skipped);
        } else {
            println!("Pushed {pushed} file(s) under '{}'{}",
                     ctx.entry.name,
                     if skipped > 0 { format!(" ({skipped} already synced)") } else { String::new() });
        }
        // Non-fatal: report refused files as a tail block so the user
        // knows which ones still need manual attention.
        if !refused.is_empty() {
            eprintln!("\n{} file(s) skipped:", refused.len());
            for (p, why) in &refused {
                eprintln!("  {p}: {why}");
            }
        }
        return Ok(());
    }

    // Single file
    match classify_for_push(ctx.entry.status) {
        PushAction::Fsync => {
            fsync_through_mount(&ctx.mount, &ctx.entry)?;
            println!("Pushed '{}'", ctx.entry.name);
        }
        PushAction::AlreadySynced => {
            println!("'{}' is already synced", ctx.entry.name);
        }
        PushAction::Refuse(reason) => {
            anyhow::bail!("{}: {}", ctx.entry.name, reason);
        }
    }
    Ok(())
}

/// Open the file at `<mount_path>/<remote_path>` for reading and call
/// `sync_all()`. The kernel forwards the fsync to the FUSE daemon's
/// callback, which enqueues `UploadTrigger::Fsync` — already wired to
/// bypass the debounce and bandwidth window.
fn fsync_through_mount(mount: &MountConfig, entry: &FileEntry) -> Result<()> {
    let mount_root = expand_tilde(&mount.resolved_mount_path());
    let rel = entry.remote_path.trim_start_matches('/');
    let full = mount_root.join(rel);

    // Read-only is enough; fsync doesn't require write permission. We
    // intentionally don't open with O_NONBLOCK because the daemon's
    // open() callback finishes immediately for files that already have
    // a local cache (Dirty/Uploading), and we've already classified
    // out the statuses where open would block on a download.
    let file = std::fs::File::open(&full)
        .with_context(|| format!("open {} for fsync", full.display()))?;
    file.sync_all()
        .with_context(|| format!("fsync {}", full.display()))?;
    Ok(())
}

// ── Path resolution (mirrors pin.rs's structure) ────────────────────────────

struct PushContext {
    db:       StateDb,
    mount_id: u32,
    entry:    FileEntry,
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

async fn resolve_path(config_path: &Path, user_path: &Path) -> Result<PushContext> {
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

    Ok(PushContext { db, mount_id, entry, mount })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Dirty/Uploading must produce Fsync — that's the whole reason
    /// `push` exists. Without this, an editor's auto-save that's stuck
    /// in the 60s retry-backoff window has no way to be flushed.
    #[test]
    fn dirty_and_uploading_trigger_fsync() {
        assert_eq!(classify_for_push(SyncStatus::Dirty),     PushAction::Fsync);
        assert_eq!(classify_for_push(SyncStatus::Uploading), PushAction::Fsync);
    }

    /// Cached is the "nothing to do" status. Returning Fsync on Cached
    /// would be harmless (the upload queue would no-op on Cached) but
    /// it's better UX to tell the user there's nothing to push.
    #[test]
    fn cached_reports_already_synced() {
        assert_eq!(classify_for_push(SyncStatus::Cached), PushAction::AlreadySynced);
    }

    /// Remote/Hydrating mean the file isn't on disk yet — pushing it
    /// is incoherent. Returning Refuse with a useful message points
    /// the user at `pin` (or just opening the file) instead.
    #[test]
    fn remote_and_hydrating_refuse_with_reason() {
        for s in [SyncStatus::Remote, SyncStatus::Hydrating] {
            match classify_for_push(s) {
                PushAction::Refuse(_) => {}
                other => panic!("expected Refuse for {s:?}, got {other:?}"),
            }
        }
    }

    /// Stale means remote has changes we haven't pulled. Force-pushing
    /// the older local copy would clobber those remote changes, which
    /// is destructive. Refuse and tell the user the path forward.
    #[test]
    fn stale_refuses_to_avoid_clobbering_remote() {
        match classify_for_push(SyncStatus::Stale) {
            PushAction::Refuse(msg) => {
                assert!(msg.contains("stale") || msg.contains("re-hydrate"),
                    "stale message should mention the staleness: {msg:?}");
            }
            other => panic!("expected Refuse for Stale, got {other:?}"),
        }
    }

    /// Conflict means there's a `.conflict.{ts}.{hash}` sibling waiting
    /// for the user. Pushing without resolving would risk turning into
    /// another conflict cycle. Refuse and point at the conflict CLI.
    #[test]
    fn conflict_refuses_and_points_to_resolver() {
        match classify_for_push(SyncStatus::Conflict) {
            PushAction::Refuse(msg) => {
                assert!(msg.contains("conflicts"),
                    "conflict message should mention the resolver: {msg:?}");
            }
            other => panic!("expected Refuse for Conflict, got {other:?}"),
        }
    }
}
