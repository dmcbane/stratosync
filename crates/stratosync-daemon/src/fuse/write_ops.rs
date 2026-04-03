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
        .map_err(|_| libc::EIO)?;

    file.seek(std::io::SeekFrom::Start(offset as u64)).await
        .map_err(|_| libc::EIO)?;
    file.write_all(data).await.map_err(|_| libc::EIO)?;
    file.flush().await.map_err(|_| libc::EIO)?;

    // Mark dirty and start debounce.
    // The FUSE write already succeeded (data is in the cache file), so we don't
    // fail the syscall if the DB update fails — but we must surface the error.
    if let Err(e) = db.set_status(entry.inode, SyncStatus::Dirty).await {
        warn!(inode = entry.inode, "set_status(Dirty) failed: {e}");
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
    // Derive parent's remote_path to build child's remote_path
    let parent_entry = db.get_by_inode(parent).await
        .map_err(|_| libc::EIO)?
        .ok_or(libc::ENOENT)?;

    let remote_path = join_remote(&parent_entry.remote_path, name);
    let cache_path  = cache_dir.join(remote_path.trim_start_matches('/'));

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

    // Invalidate parent directory listing so the next readdir re-fetches.
    // The create itself already succeeded, so this is best-effort.
    if let Err(e) = db.invalidate_dir(parent).await {
        warn!(inode, parent, "invalidate_dir after create failed: {e}");
    }

    // Allocate file handle
    let fh = next_fh.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    open_files.insert(fh, super::OpenFile {
        inode,
        cache_path,
        flags: libc::O_WRONLY,
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
    let parent_entry = db.get_by_inode(parent).await
        .map_err(|_| libc::EIO)?
        .ok_or(libc::ENOENT)?;

    let remote_path = join_remote(&parent_entry.remote_path, name);

    // Create remotely
    backend.mkdir(&remote_path).await.map_err(|_| libc::EIO)?;

    // Insert DB entry
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

    if let Err(e) = db.invalidate_dir(parent).await {
        warn!(inode, parent, "invalidate_dir after mkdir failed: {e}");
    }
    Ok(inode)
}

// ── unlink ────────────────────────────────────────────────────────────────────

pub async fn handle_unlink(
    parent:  Inode,
    name:    &str,
    db:      &Arc<StateDb>,
    backend: &Arc<dyn Backend>,
) -> Result<(), libc::c_int> {
    let entry = db.get_by_parent_name(parent, name).await
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

    // Remove DB entry first so the file disappears from listings immediately.
    let remote_path = entry.remote_path.clone();
    db.delete_entry(entry.inode).await.map_err(|_| libc::EIO)?;
    if let Err(e) = db.invalidate_dir(parent).await {
        warn!(parent, "invalidate_dir after unlink failed: {e}");
    }

    // Delete remotely in background — don't block the FUSE thread.
    // NotFound is fine (file may not exist remotely, e.g. never uploaded).
    let backend = Arc::clone(backend);
    tokio::spawn(async move {
        match backend.delete(&remote_path).await {
            Ok(()) | Err(SyncError::NotFound(_)) => {}
            Err(e) => warn!(path = %remote_path, "background remote delete failed: {e}"),
        }
    });

    Ok(())
}

// ── rmdir ─────────────────────────────────────────────────────────────────────

pub async fn handle_rmdir(
    parent:  Inode,
    name:    &str,
    db:      &Arc<StateDb>,
    backend: &Arc<dyn Backend>,
) -> Result<(), libc::c_int> {
    let entry = db.get_by_parent_name(parent, name).await
        .map_err(|_| libc::EIO)?
        .ok_or(libc::ENOENT)?;

    if entry.kind != FileKind::Directory { return Err(libc::ENOTDIR); }

    // Check for children — refuse to rmdir non-empty
    let children = db.list_children(entry.inode).await.map_err(|_| libc::EIO)?;
    if !children.is_empty() { return Err(libc::ENOTEMPTY); }

    let remote_path = entry.remote_path.clone();
    db.delete_entry(entry.inode).await.map_err(|_| libc::EIO)?;
    if let Err(e) = db.invalidate_dir(parent).await {
        warn!(parent, "invalidate_dir after rmdir failed: {e}");
    }

    // Delete remotely in background
    let backend = Arc::clone(backend);
    tokio::spawn(async move {
        match backend.delete(&remote_path).await {
            Ok(()) | Err(SyncError::NotFound(_)) => {}
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
    db:         &Arc<StateDb>,
    backend:    &Arc<dyn Backend>,
) -> Result<(), libc::c_int> {
    let entry = db.get_by_parent_name(parent, name).await
        .map_err(|_| libc::EIO)?
        .ok_or(libc::ENOENT)?;

    let new_parent_entry = db.get_by_inode(new_parent).await
        .map_err(|_| libc::EIO)?
        .ok_or(libc::ENOENT)?;

    let new_remote = join_remote(&new_parent_entry.remote_path, new_name);

    // If destination exists, delete it first (POSIX rename semantics)
    if let Ok(Some(dest)) = db.get_by_parent_name(new_parent, new_name).await {
        match backend.delete(&dest.remote_path).await {
            Ok(()) | Err(SyncError::NotFound(_)) => {}
            Err(_) => return Err(libc::EIO),
        }
        if let Err(e) = db.delete_entry(dest.inode).await {
            warn!(inode = dest.inode, "delete_entry for rename overwrite failed: {e}");
        }
    }

    // Move on the remote
    backend.rename(&entry.remote_path, &new_remote).await
        .map_err(|_| libc::EIO)?;

    // Update local cache path if hydrated.
    // Failure here means the cache file is at the old path; next open()
    // will re-hydrate from the (already-renamed) remote path.
    if let Some(old_cache) = &entry.cache_path {
        let new_cache = old_cache.parent()
            .map(|p| p.join(new_name))
            .unwrap_or_else(|| PathBuf::from(new_name));
        if let Err(e) = tokio::fs::rename(old_cache, &new_cache).await {
            warn!(inode = entry.inode, ?old_cache, ?new_cache, "cache rename failed: {e}");
        }
    }

    db.rename_entry(entry.inode, new_parent, new_name, &new_remote).await
        .map_err(|_| libc::EIO)?;

    if let Err(e) = db.invalidate_dir(parent).await {
        warn!(parent, "invalidate_dir after rename (src) failed: {e}");
    }
    if new_parent != parent {
        if let Err(e) = db.invalidate_dir(new_parent).await {
            warn!(new_parent, "invalidate_dir after rename (dst) failed: {e}");
        }
    }

    Ok(())
}

// ── helpers ───────────────────────────────────────────────────────────────────

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
