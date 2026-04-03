#![allow(dead_code, unused_imports)]
/// ConflictResolver — handles concurrent write collisions.
///
/// Algorithm: remote wins the canonical path; local version is uploaded
/// under a `.conflict.{ts}.{hash}.{ext}` sibling name.
/// See docs/architecture/06-conflict-resolution.md for full design.
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use chrono::Utc;
use tracing::{debug, info, warn};

use stratosync_core::{
    backend::Backend,
    state::StateDb,
    types::{FileEntry, FileKind, SyncStatus},
};
use stratosync_core::state::NewFileEntry;

// ── Public entry point ────────────────────────────────────────────────────────

/// Called when an upload returns `SyncError::Conflict`.
///
/// Steps:
///  1. Compute a unique conflict filename.
///  2. Upload the local (losing) version under that name.
///  3. Download the remote (winning) version back into local cache.
///  4. Update the DB: canonical inode ← remote, new inode ← conflict file.
///  5. Emit a desktop notification if possible.
pub async fn resolve(
    entry:   &FileEntry,
    db:      &Arc<StateDb>,
    backend: &Arc<dyn Backend>,
) -> Result<()> {
    let cache_path = match &entry.cache_path {
        Some(p) => p.clone(),
        None => {
            warn!(inode = entry.inode, "conflict resolve called but no cache_path");
            return Ok(());
        }
    };

    // ── 1. Build conflict filename ────────────────────────────────────────────
    let conflict_name = make_conflict_name(&entry.name, &cache_path);
    let conflict_remote = sibling_path(&entry.remote_path, &conflict_name);

    info!(
        inode = entry.inode,
        canonical = %entry.remote_path,
        conflict  = %conflict_remote,
        "resolving conflict"
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
