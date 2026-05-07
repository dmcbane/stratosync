//! `stratosync cache` — operator recovery commands for the local cache.
//!
//! Today this is just `clear`: detach cached files from the index, unlink
//! them from disk, leave Dirty/Uploading/Conflict/Pinned rows alone, and
//! the next `open()` re-hydrates from the remote. Use when local state
//! has drifted out of sync with the cloud and you want to reset without
//! losing in-flight work.
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use bytesize::ByteSize;
use stratosync_core::{
    config::{default_data_dir, default_runtime_socket, MountConfig},
    state::{CacheClearReport, StateDb},
};
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::time::timeout;

/// Top-level entry: clear the cache for one mount or all of them.
pub async fn clear(
    config_path:    &Path,
    mount_filter:   Option<&str>,
    all:            bool,
    include_pinned: bool,
    force:          bool,
    yes:            bool,
) -> Result<()> {
    if mount_filter.is_some() && all {
        bail!("--mount and --all are mutually exclusive");
    }
    if mount_filter.is_none() && !all {
        bail!("specify --mount NAME or --all");
    }

    let cfg = crate::config_io::load(config_path)?;
    let mounts: Vec<MountConfig> = if let Some(name) = mount_filter {
        let m = cfg.mounts.iter().find(|m| m.name == name).cloned()
            .ok_or_else(|| anyhow::anyhow!("no mount named '{name}' in config"))?;
        vec![m]
    } else {
        cfg.mounts.iter().filter(|m| m.enabled).cloned().collect()
    };
    if mounts.is_empty() {
        bail!("no mounts to clear (config has none enabled)");
    }

    if daemon_is_running().await {
        if force {
            eprintln!("warning: daemon is running — proceeding because --force was given.");
            eprintln!("         in-flight hydrations / writes may misbehave until restart.");
        } else {
            bail!(
                "daemon is running — stop it first with `stratosync daemon stop`,\n\
                 or pass --force to override (not recommended; live FUSE handles\n\
                 may end up pointing at deleted cache files)."
            );
        }
    }

    if !yes && !confirm(&mounts, include_pinned)? {
        println!("aborted.");
        return Ok(());
    }

    let mut total = CacheClearReport::default();
    for m in &mounts {
        let report = clear_one(m, include_pinned).await
            .with_context(|| format!("clear cache for mount '{}'", m.name))?;
        println!(
            "[{}] cleared {} file(s), freed {}",
            m.name,
            report.files_cleared,
            ByteSize(report.bytes_freed),
        );
        total.files_cleared += report.files_cleared;
        total.bytes_freed   += report.bytes_freed;
    }
    if mounts.len() > 1 {
        println!(
            "total: {} file(s), {} freed across {} mount(s)",
            total.files_cleared,
            ByteSize(total.bytes_freed),
            mounts.len(),
        );
    }
    Ok(())
}

async fn clear_one(mount: &MountConfig, include_pinned: bool) -> Result<CacheClearReport> {
    let db_path = default_data_dir().join(format!("{}.db", mount.name));
    if !db_path.exists() {
        // Nothing has ever been indexed for this mount — treat as a no-op
        // success rather than failing. The on-disk cache directory might
        // still exist from a manually-aborted run, so still try to clean it.
        let mut report = CacheClearReport::default();
        let stray = wipe_cache_dir(&mount.cache_dir())?;
        report.bytes_freed = stray;
        return Ok(report);
    }

    let db = StateDb::open(&db_path)
        .with_context(|| format!("open state DB {}", db_path.display()))?;
    let mount_id = db.get_mount_id(&mount.name).await?
        .ok_or_else(|| anyhow::anyhow!(
            "mount '{}' is in config but not in DB (never indexed yet)", mount.name
        ))?;

    let report = db.clear_cache_for_mount(mount_id, include_pinned).await?;

    // Unlink the cache files we just detached from the index. Best-effort:
    // a file that's already gone (race with eviction, or a partially-applied
    // earlier run) is fine. Other I/O errors bubble up.
    for p in &report.cache_paths {
        match std::fs::remove_file(p) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("unlink {}", p.display())),
        }
    }

    // Sweep empty parent dirs left behind so the cache tree doesn't
    // accumulate skeleton directories over time.
    prune_empty_dirs(&mount.cache_dir(), &report.cache_paths);

    Ok(report)
}

fn confirm(mounts: &[MountConfig], include_pinned: bool) -> Result<bool> {
    use std::io::{self, BufRead, Write};
    let names: Vec<&str> = mounts.iter().map(|m| m.name.as_str()).collect();
    println!("About to clear local cache for: {}", names.join(", "));
    println!("  Files with status `cached` and `stale` will be re-hydrated on next access.");
    println!("  Dirty/Uploading/Conflict files are NOT touched (unsynced work is preserved).");
    if include_pinned {
        println!("  --include-pinned: pinned files will ALSO be cleared.");
    } else {
        println!("  Pinned files are skipped (pass --include-pinned to override).");
    }
    print!("Continue? [y/N] ");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    Ok(matches!(line.trim(), "y" | "Y" | "yes" | "YES"))
}

/// Best-effort: walk the cache directory and `rmdir` any empty parents
/// of the files we just unlinked. We stop at the cache root so we never
/// remove the mount's cache directory itself.
fn prune_empty_dirs(cache_root: &Path, cleared: &[PathBuf]) {
    use std::collections::BTreeSet;
    let mut parents = BTreeSet::new();
    for p in cleared {
        let mut cur = p.parent();
        while let Some(dir) = cur {
            if dir == cache_root || !dir.starts_with(cache_root) { break; }
            parents.insert(dir.to_path_buf());
            cur = dir.parent();
        }
    }
    // Deepest-first so children disappear before their parents.
    for dir in parents.iter().rev() {
        let _ = std::fs::remove_dir(dir);
    }
}

/// Recursively delete the contents of `cache_dir`. Used only for the
/// "DB doesn't exist but cache might" edge case in `clear_one`.
/// Returns total bytes removed (best-effort tally).
fn wipe_cache_dir(cache_dir: &Path) -> Result<u64> {
    if !cache_dir.exists() { return Ok(0); }
    let mut bytes = 0u64;
    for entry in walkdir(cache_dir) {
        if let Ok(meta) = entry.metadata() {
            if meta.is_file() {
                bytes += meta.len();
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    Ok(bytes)
}

fn walkdir(root: &Path) -> Vec<DirEntry> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for entry in rd.flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p.clone());
            }
            out.push(DirEntry { path: p });
        }
    }
    out
}

struct DirEntry { path: PathBuf }
impl DirEntry {
    fn metadata(&self) -> std::io::Result<std::fs::Metadata> { std::fs::metadata(&self.path) }
    fn path(&self) -> &Path { &self.path }
}

/// Probe the daemon's IPC socket to decide whether the daemon is up.
/// A successful connect (regardless of the response) means a process is
/// listening on the socket — that's our "running" signal.
async fn daemon_is_running() -> bool {
    let sock = default_runtime_socket();
    match timeout(Duration::from_millis(500), UnixStream::connect(&sock)).await {
        Ok(Ok(mut s)) => {
            let _ = s.shutdown().await;
            true
        }
        _ => false,
    }
}
