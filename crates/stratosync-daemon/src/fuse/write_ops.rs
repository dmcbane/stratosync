#![allow(unused_imports, dead_code)]
/// Phase 2 FUSE write operations.
///
/// These are split into a separate file to keep fuse/mod.rs readable.
/// They are `impl`-ed on `StratoFs` via a trait and called from the
/// main `Filesystem` impl.
///
/// Write flow:
///   write()   → pwrite(cache_fd) → mark DIRTY → debounce timer starts
///   close()   → short debounce
///   fsync()   → immediate upload (blocks until complete)
///   create()  → new cache file + new DB entry + DIRTY
///   mkdir()   → new DB entry (dir, REMOTE) + enqueue backend::mkdir
///   unlink()  → DB delete + enqueue backend::delete
///   rmdir()   → DB delete + enqueue backend::delete
///   rename()  → DB rename + enqueue backend::rename
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use anyhow::Result;
use dashmap::DashMap;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio::sync::oneshot;
use tracing::{debug, error, warn};

use stratosync_core::{
    state::{NewFileEntry, StateDb},
    types::{FileEntry, FileKind, Inode, SyncError, SyncStatus},
    backend::Backend,
};
use crate::sync::upload_queue::{UploadQueue, UploadTrigger};

/// Map IO errors to appropriate FUSE errno. Distinguishes disk-full from generic IO.
fn io_errno(e: &std::io::Error) -> libc::c_int {
    match e.raw_os_error() {
        Some(libc::ENOSPC) => libc::ENOSPC,
        Some(libc::EDQUOT) => libc::ENOSPC,
        Some(code)         => code,
        None               => libc::EIO,
    }
}

/// Context passed to each write operation.
pub struct WriteCtx {
    pub mount_id:     u32,
    pub db:           Arc<StateDb>,
    pub backend:      Arc<dyn Backend>,
    pub upload_queue: Arc<UploadQueue>,
    pub cache_dir:    PathBuf,
}

// ── write ─────────────────────────────────────────────────────────────────────

pub async fn handle_write(
    fh:         u64,
    offset:     i64,
    data:       &[u8],
    open_files: &Arc<DashMap<u64, super::OpenFile>>,
    db:         &Arc<StateDb>,
    queue:      &Arc<UploadQueue>,
) -> Result<u32, libc::c_int> {
    let entry = open_files.get(&fh).ok_or(libc::EBADF)?;

    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .open(&entry.cache_path).await
        .map_err(|e| io_errno(&e))?;

    file.seek(std::io::SeekFrom::Start(offset as u64)).await
        .map_err(|e| io_errno(&e))?;
    file.write_all(data).await.map_err(|e| io_errno(&e))?;
    file.flush().await.map_err(|e| io_errno(&e))?;

    // Get the new file size so getattr reports it correctly.
    let new_size = file.metadata().await.map(|m| m.len()).unwrap_or(0);

    // Mark dirty and update size. The FUSE write already succeeded (data is
    // in the cache file), so we don't fail the syscall if the DB update
    // fails — but we must surface the error.
    if let Err(e) = db.set_dirty_size(entry.inode, new_size).await {
        warn!(inode = entry.inode, "set_dirty_size failed: {e}");
    }
    queue.enqueue(UploadTrigger::Write { inode: entry.inode }).await;

    Ok(data.len() as u32)
}

// ── fsync ─────────────────────────────────────────────────────────────────────

pub async fn handle_fsync(
    fh:         u64,
    open_files: &Arc<DashMap<u64, super::OpenFile>>,
    queue:      &Arc<UploadQueue>,
) -> Result<(), libc::c_int> {
    let entry = open_files.get(&fh).ok_or(libc::EBADF)?;
    queue.enqueue(UploadTrigger::Fsync { inode: entry.inode }).await;
    Ok(())
}

// ── release (close) ───────────────────────────────────────────────────────────

pub async fn handle_release(
    fh:         u64,
    open_files: &Arc<DashMap<u64, super::OpenFile>>,
    db:         &Arc<StateDb>,
    queue:      &Arc<UploadQueue>,
) {
    if let Some((_, entry)) = open_files.remove(&fh) {
        // If dirty, trigger close-debounce upload
        if let Ok(Some(fe)) = db.get_by_inode(entry.inode).await {
            if matches!(fe.status, SyncStatus::Dirty) {
                queue.enqueue(UploadTrigger::Close { inode: entry.inode }).await;
            }
        }
    }
}

// ── create ────────────────────────────────────────────────────────────────────

pub async fn handle_create(
    parent:       Inode,
    name:         &str,
    mount_id:     u32,
    db:           &Arc<StateDb>,
    cache_dir:    &Path,
    open_files:   &Arc<DashMap<u64, super::OpenFile>>,
    next_fh:      &Arc<std::sync::atomic::AtomicU64>,
) -> Result<(Inode, u64), libc::c_int> {
    validate_filename(name)?;

    let parent_entry = db.get_by_inode(parent).await
        .map_err(|_| libc::EIO)?
        .ok_or(libc::ENOENT)?;

    let remote_path = join_remote(&parent_entry.remote_path, name);
    let cache_path  = safe_cache_path(cache_dir, &remote_path)?;

    // Create the empty cache file
    if let Some(parent_dir) = cache_path.parent() {
        tokio::fs::create_dir_all(parent_dir).await.map_err(|_| libc::EIO)?;
    }
    tokio::fs::File::create(&cache_path).await.map_err(|_| libc::EIO)?;

    // Insert DB entry (DIRTY — will upload on close/fsync)
    let inode = db.insert_file(&NewFileEntry {
        mount_id,
        parent,
        name:        name.to_owned(),
        remote_path: remote_path.clone(),
        kind:        FileKind::File,
        size:        0,
        mtime:       SystemTime::now(),
        etag:        None,
        status:      SyncStatus::Dirty,
        cache_path:  Some(cache_path.clone()),
        cache_size:  Some(0),
    }).await.map_err(|_| libc::EIO)?;

    // No dir invalidation needed — the DB insert above already makes this
    // file visible in list_children. The poller handles remote changes.

    // Allocate file handle
    let fh = next_fh.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    open_files.insert(fh, super::OpenFile {
        inode,
        cache_path,
        remote_path: remote_path.to_owned(),
        flags: libc::O_WRONLY,
        hydrating: false,
    });

    Ok((inode, fh))
}

// ── mkdir ─────────────────────────────────────────────────────────────────────

pub async fn handle_mkdir(
    parent:   Inode,
    name:     &str,
    mount_id: u32,
    db:       &Arc<StateDb>,
    backend:  &Arc<dyn Backend>,
) -> Result<Inode, libc::c_int> {
    validate_filename(name)?;

    let parent_entry = db.get_by_inode(parent).await
        .map_err(|_| libc::EIO)?
        .ok_or(libc::ENOENT)?;

    let remote_path = join_remote(&parent_entry.remote_path, name);

    // Insert DB entry immediately so the directory is visible in listings
    let inode = db.insert_file(&NewFileEntry {
        mount_id,
        parent,
        name:        name.to_owned(),
        remote_path: remote_path.clone(),
        kind:        FileKind::Directory,
        size:        0,
        mtime:       SystemTime::now(),
        etag:        None,
        status:      SyncStatus::Cached,
        cache_path:  None,
        cache_size:  None,
    }).await.map_err(|_| libc::EIO)?;

    // Create remotely in background. If this fails, uploads inside the dir
    // will implicitly create the parent on most cloud backends.
    let backend = Arc::clone(backend);
    tokio::spawn(async move {
        if let Err(e) = backend.mkdir(&remote_path).await {
            warn!(path = %remote_path, "background remote mkdir failed: {e}");
        }
    });

    Ok(inode)
}

// ── unlink ────────────────────────────────────────────────────────────────────

pub async fn handle_unlink(
    parent:   Inode,
    name:     &str,
    mount_id: u32,
    db:       &Arc<StateDb>,
    backend:  &Arc<dyn Backend>,
) -> Result<(), libc::c_int> {
    let entry = db.get_by_parent_name(mount_id, parent, name).await
        .map_err(|_| libc::EIO)?
        .ok_or(libc::ENOENT)?;

    // Remove local cache file if present.
    // ENOENT is acceptable (file may not have been hydrated).
    if let Some(cp) = &entry.cache_path {
        if let Err(e) = tokio::fs::remove_file(cp).await {
            if e.kind() != std::io::ErrorKind::NotFound {
                warn!(inode = entry.inode, path = ?cp, "cache file removal failed: {e}");
            }
        }
    }

    // Remove DB entry and insert tombstone so the poller doesn't re-add it
    // before the background remote delete completes.
    let remote_path = entry.remote_path.clone();
    db.delete_entry(entry.inode).await.map_err(|_| libc::EIO)?;
    if let Err(e) = db.insert_tombstone(mount_id, &remote_path, 300).await {
        warn!(path = %remote_path, "insert_tombstone failed: {e}");
    }

    // Delete remotely in background. Remove tombstone on success.
    let backend = Arc::clone(backend);
    let db = Arc::clone(db);
    tokio::spawn(async move {
        match backend.delete(&remote_path).await {
            Ok(()) | Err(SyncError::NotFound(_)) => {
                let _ = db.remove_tombstone(mount_id, &remote_path).await;
            }
            Err(e) => warn!(path = %remote_path, "background remote delete failed: {e}"),
        }
    });

    Ok(())
}

// ── rmdir ─────────────────────────────────────────────────────────────────────

pub async fn handle_rmdir(
    parent:   Inode,
    name:     &str,
    mount_id: u32,
    db:       &Arc<StateDb>,
    backend:  &Arc<dyn Backend>,
) -> Result<(), libc::c_int> {
    let entry = db.get_by_parent_name(mount_id, parent, name).await
        .map_err(|_| libc::EIO)?
        .ok_or(libc::ENOENT)?;

    if entry.kind != FileKind::Directory { return Err(libc::ENOTDIR); }

    // Check for children — refuse to rmdir non-empty
    let children = db.list_children(mount_id, entry.inode).await.map_err(|_| libc::EIO)?;
    if !children.is_empty() { return Err(libc::ENOTEMPTY); }

    let remote_path = entry.remote_path.clone();
    db.delete_entry(entry.inode).await.map_err(|_| libc::EIO)?;
    // Directory tombstone also blocks children (e.g. rm -rf dir/)
    if let Err(e) = db.insert_tombstone(mount_id, &remote_path, 300).await {
        warn!(path = %remote_path, "insert_tombstone failed: {e}");
    }

    // Remove remote directory in background. Remove tombstone on success.
    let backend = Arc::clone(backend);
    let db = Arc::clone(db);
    tokio::spawn(async move {
        match backend.rmdir(&remote_path).await {
            Ok(()) | Err(SyncError::NotFound(_)) => {
                let _ = db.remove_tombstone(mount_id, &remote_path).await;
            }
            Err(e) => warn!(path = %remote_path, "background remote rmdir failed: {e}"),
        }
    });

    Ok(())
}

// ── rename ────────────────────────────────────────────────────────────────────

pub async fn handle_rename(
    parent:     Inode,
    name:       &str,
    new_parent: Inode,
    new_name:   &str,
    mount_id:   u32,
    db:         &Arc<StateDb>,
    backend:    &Arc<dyn Backend>,
) -> Result<(), libc::c_int> {
    validate_filename(new_name)?;

    let entry = db.get_by_parent_name(mount_id, parent, name).await
        .map_err(|_| libc::EIO)?
        .ok_or(libc::ENOENT)?;

    let new_parent_entry = db.get_by_inode(new_parent).await
        .map_err(|_| libc::EIO)?
        .ok_or(libc::ENOENT)?;

    let new_remote = join_remote(&new_parent_entry.remote_path, new_name);

    // If destination exists, delete it from DB (POSIX rename semantics)
    let dest_remote = if let Ok(Some(dest)) = db.get_by_parent_name(mount_id, new_parent, new_name).await {
        let rp = dest.remote_path.clone();
        if let Err(e) = db.delete_entry(dest.inode).await {
            warn!(inode = dest.inode, "delete_entry for rename overwrite failed: {e}");
        }
        Some(rp)
    } else {
        None
    };

    // Update local cache path if hydrated.
    let new_cache_path = if let Some(old_cache) = &entry.cache_path {
        let new_cache = old_cache.parent()
            .map(|p| p.join(new_name))
            .unwrap_or_else(|| PathBuf::from(new_name));
        if let Err(e) = tokio::fs::rename(old_cache, &new_cache).await {
            warn!(inode = entry.inode, ?old_cache, ?new_cache, "cache rename failed: {e}");
            Some(old_cache.clone())
        } else {
            Some(new_cache)
        }
    } else {
        None
    };

    // Update DB immediately so the file appears at its new location
    db.rename_entry(entry.inode, new_parent, new_name, &new_remote,
        new_cache_path.as_deref()).await
        .map_err(|_| libc::EIO)?;

    // Queue remote operations in background
    let needs_remote_rename = matches!(
        entry.status,
        SyncStatus::Cached | SyncStatus::Stale | SyncStatus::Uploading | SyncStatus::Conflict
    );

    if needs_remote_rename || dest_remote.is_some() {
        let old_remote = entry.remote_path.clone();
        let new_remote_bg = new_remote.clone();
        let backend = Arc::clone(backend);
        let db = Arc::clone(db);

        // Tombstone the old path so poller doesn't re-add it
        if needs_remote_rename {
            let _ = db.insert_tombstone(mount_id, &old_remote, 300).await;
        }

        tokio::spawn(async move {
            // Delete destination if it existed on remote
            if let Some(ref dest_rp) = dest_remote {
                match backend.delete(dest_rp).await {
                    Ok(()) | Err(SyncError::NotFound(_)) => {}
                    Err(e) => warn!(path = %dest_rp, "background rename dest delete failed: {e}"),
                }
            }
            // Rename on remote
            if needs_remote_rename {
                match backend.rename(&old_remote, &new_remote_bg).await {
                    Ok(()) | Err(SyncError::NotFound(_)) => {}
                    Err(e) => warn!(from = %old_remote, to = %new_remote_bg, "background rename failed: {e}"),
                }
                let _ = db.remove_tombstone(mount_id, &old_remote).await;
            }
        });
    }

    Ok(())
}

// ── helpers ───────────────────────────────────────────────────────────────────

/// Validate a filename from a FUSE operation or rclone listing.
/// Rejects path traversal components, embedded slashes, and null bytes.
pub fn validate_filename(name: &str) -> Result<(), libc::c_int> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.contains('\0')
    {
        warn!(name, "rejected invalid filename");
        return Err(libc::EINVAL);
    }
    Ok(())
}

/// Construct a cache path and verify it stays within the cache directory.
/// Prevents path traversal via ".." in remote_path components.
pub fn safe_cache_path(cache_dir: &Path, remote_path: &str) -> Result<PathBuf, libc::c_int> {
    let rel = remote_path.trim_start_matches('/');
    let candidate = cache_dir.join(rel);
    // Resolve ".." and symlinks — the canonical path must still be under cache_dir
    // For files that don't exist yet, canonicalize the parent directory
    let check_dir = if candidate.exists() {
        match candidate.canonicalize() {
            Ok(p) => p,
            Err(_) => return Err(libc::EIO),
        }
    } else {
        let parent = candidate.parent().unwrap_or(cache_dir);
        match parent.canonicalize() {
            Ok(p) => p.join(candidate.file_name().unwrap_or_default()),
            Err(_) => candidate.clone(),
        }
    };
    let canon_cache = cache_dir.canonicalize().unwrap_or_else(|_| cache_dir.to_path_buf());
    if !check_dir.starts_with(&canon_cache) {
        warn!(path = ?candidate, "path traversal attempt — escapes cache dir");
        return Err(libc::EACCES);
    }
    Ok(candidate)
}

/// Concatenate a parent remote path with a child name.
/// Returns paths without a leading slash to match rclone's lsjson output.
/// `join_remote("/", "notes.md")` → `"notes.md"`
/// `join_remote("Documents", "notes.md")` → `"Documents/notes.md"`
pub fn join_remote(parent: &str, child: &str) -> String {
    let p = parent.trim_matches('/');
    if p.is_empty() {
        child.to_string()
    } else {
        format!("{p}/{child}")
    }
}
