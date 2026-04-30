#![allow(unused_imports, unused_variables, dead_code)]
/// FUSE filesystem implementation — Phase 1 (read) + Phase 2 (write).
/// See docs/architecture/02-fuse-layer.md for design details.
pub mod header_cache;
pub mod write_ops;

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::sync::atomic::AtomicU64;
use std::time::SystemTime;

use anyhow::Context;
use dashmap::DashMap;
use fuser::{
    FileAttr, FileType, Filesystem, ReplyAttr, ReplyCreate, ReplyData,
    ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite,
    ReplyXattr, Request,
};
use tokio::runtime::Handle;
use tokio::sync::oneshot;
#[allow(unused_imports)]
use tracing::{debug, error, warn};

use stratosync_core::{
    backend::Backend, base_store::BaseStore,
    config::{FuseConfig, SyncConfig},
    state::{NewFileEntry, StateDb}, types::*, GlobSet,
};
use crate::sync::upload_queue::{UploadQueue, UploadTrigger};

pub struct OpenFile {
    pub inode:       Inode,
    pub cache_path:  PathBuf,
    pub remote_path: String,
    pub flags:       i32,
    pub hydrating:   bool,
}

// Extended attributes exposed as read-only metadata.
const XATTR_STATUS: &[u8]      = b"user.stratosync.status";
const XATTR_ETAG: &[u8]        = b"user.stratosync.etag";
const XATTR_REMOTE_PATH: &[u8] = b"user.stratosync.remote_path";

pub struct StratoFs {
    pub mount_id:          u32,
    pub mount_name:        String,
    pub db:                Arc<StateDb>,
    pub backend:           Arc<dyn Backend>,
    pub base_store:        Arc<BaseStore>,
    pub sync_config:       Arc<SyncConfig>,
    pub cache_dir:         PathBuf,
    pub cfg:               FuseConfig,
    pub rt:                Handle,
    pub open_files:        Arc<DashMap<u64, OpenFile>>,
    pub next_fh:           Arc<AtomicU64>,
    pub hydration_waiters: Arc<DashMap<Inode, Vec<oneshot::Sender<Result<(), libc::c_int>>>>>,
    pub upload_queue:      Arc<UploadQueue>,
    pub ignore:            Arc<GlobSet>,
    /// Kernel-cache invalidator. Populated once `Session::new` is built —
    /// before that the slot is empty and any invalidation is a no-op (the
    /// kernel hasn't cached anything yet anyway). Used after directory
    /// mutations (mkdir/create/unlink/rmdir/rename) so KIO/Dolphin's
    /// readdir cache for the parent gets dropped and the new entry shows
    /// up without the user navigating away and back.
    pub notifier:          Arc<OnceLock<fuser::Notifier>>,
    /// Inodes that currently have a header prefetch (or opportunistic
    /// cache write) in flight. Lets concurrent reads / prefetch passes
    /// dedupe — without this, 11 simultaneous KIO sniffs on the same
    /// file each spawn their own range download, and the prefetch pass
    /// piles on top.
    pub prefetch_inflight: Arc<DashMap<Inode, ()>>,
    /// Daemon-wide cap on concurrent header pre-fetches. Without a
    /// shared semaphore, every readdir spawns its own task pool and a
    /// busy file manager (Dolphin tree view + breadcrumb panel) can
    /// kick off hundreds of concurrent rclone-cat invocations at once.
    pub prefetch_sem:      Arc<tokio::sync::Semaphore>,
    /// Inode of the directory whose prefetch we currently care about.
    /// Updated on every readdir; pre-fetch tasks check it after waking
    /// from the semaphore and bail out if the user has navigated to a
    /// different directory in the meantime. Without this, KIO walking
    /// 30 dirs through its tree-view + breadcrumb panel queues
    /// hundreds of headers, and the user's actual target lands at the
    /// back of that queue — minutes after they've right-clicked.
    pub prefetch_focus_dir: Arc<std::sync::atomic::AtomicU64>,
}

impl StratoFs {
    /// Drop the kernel's cached data for `parent_inode` so the next readdir
    /// request comes back to us and reflects DB changes we just made.
    /// Errors are logged but never surfaced — invalidation is a hint, not
    /// a correctness boundary.
    fn invalidate_dir_cache(&self, parent_inode: Inode) {
        let Some(n) = self.notifier.get() else { return };
        if let Err(e) = n.inval_inode(parent_inode, 0, 0) {
            // ENOENT means the kernel had nothing cached for this inode,
            // which is fine; other errors are worth a debug log.
            debug!(parent_inode, "inval_inode: {e}");
        }
    }
}

fn entry_to_attr(e: &FileEntry) -> FileAttr {
    let kind = match e.kind {
        FileKind::Directory => FileType::Directory,
        FileKind::Symlink   => FileType::Symlink,
        FileKind::File      => FileType::RegularFile,
    };
    let size = if e.kind == FileKind::File { e.cache_size.unwrap_or(e.size) } else { e.size };
    let mtime = e.mtime;
    FileAttr {
        ino: e.inode, size, blocks: (size + 511) / 512,
        atime: mtime, mtime, ctime: mtime, crtime: mtime, kind,
        perm: if e.kind == FileKind::Directory { 0o755 } else { 0o644 },
        nlink: 1,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        rdev: 0, blksize: 4096, flags: 0,
    }
}

/// Host-filesystem stats reported back to the kernel for `statfs(2)`.
/// Returned in the units fuser's `ReplyStatfs::statfs` expects.
///
/// Without this, fuser's default `statfs` returns zeros for every block
/// and inode count — and Dolphin's KIO copy-job pre-flights free space
/// via `statvfs(2)` and refuses to paste with "not enough room on the
/// device." Nautilus and `cp` skip the pre-flight, which is why the
/// symptom is Dolphin-specific.
#[derive(Debug)]
pub struct StatfsTotals {
    pub blocks:  u64,
    pub bfree:   u64,
    pub bavail:  u64,
    pub files:   u64,
    pub ffree:   u64,
    pub bsize:   u32,
    pub namelen: u32,
    pub frsize:  u32,
}

pub fn host_statfs(path: &std::path::Path) -> std::io::Result<StatfsTotals> {
    use std::ffi::CString;
    let cpath = CString::new(path.as_os_str().as_bytes())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let mut buf: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(cpath.as_ptr(), &mut buf) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(StatfsTotals {
        blocks:  buf.f_blocks  as u64,
        bfree:   buf.f_bfree   as u64,
        bavail:  buf.f_bavail  as u64,
        files:   buf.f_files   as u64,
        ffree:   buf.f_ffree   as u64,
        bsize:   buf.f_bsize   as u32,
        namelen: buf.f_namemax as u32,
        frsize:  buf.f_frsize  as u32,
    })
}

fn errno(e: &SyncError) -> libc::c_int {
    match e {
        SyncError::NotFound(_)         => libc::ENOENT,
        SyncError::PermissionDenied(_) => libc::EACCES,
        SyncError::QuotaExceeded       => libc::ENOSPC,
        SyncError::Network(_)          => libc::EHOSTUNREACH,
        SyncError::Transient(_)        => libc::EAGAIN,
        SyncError::Conflict { .. }     => libc::EEXIST,
        SyncError::NotSupported        => libc::ENOTSUP,
        SyncError::Io(io)              => io.raw_os_error().unwrap_or(libc::EIO),
        _                              => libc::EIO,
    }
}

pub async fn hydrate_if_needed(
    db: &Arc<StateDb>, backend: &Arc<dyn Backend>, cache_dir: &PathBuf,
    inode: Inode,
    waiters: &Arc<DashMap<Inode, Vec<oneshot::Sender<Result<(), libc::c_int>>>>>,
    base_store: &Arc<BaseStore>, sync_config: &Arc<SyncConfig>,
) -> Result<(), SyncError> {
    const MAX_RETRIES: u32 = 3;
    const WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

    for attempt in 0..MAX_RETRIES {
        let entry = db.get_by_inode(inode).await?
            .ok_or_else(|| SyncError::NotFound(format!("inode {inode}")))?;
        match entry.status {
            SyncStatus::Cached | SyncStatus::Dirty
            | SyncStatus::Uploading | SyncStatus::Conflict => return Ok(()),
            SyncStatus::Remote | SyncStatus::Stale => {
                db.set_status(inode, SyncStatus::Hydrating).await
                    .map_err(|e| SyncError::Fatal(e.to_string()))?;
                return do_hydrate(db, backend, cache_dir, waiters, &entry, base_store, sync_config).await;
            }
            SyncStatus::Hydrating => {
                let (tx, rx) = oneshot::channel();
                waiters.entry(inode).or_default().push(tx);
                match tokio::time::timeout(WAIT_TIMEOUT, rx).await {
                    Ok(Ok(Ok(()))) => return Ok(()),
                    Ok(Ok(Err(e))) => return Err(SyncError::Transient(format!("hydration failed: {e}"))),
                    Ok(Err(_)) => {
                        // Sender dropped — retry (download may have crashed)
                    }
                    Err(_) => {
                        // Timeout — reset to Remote so next attempt retries download
                        warn!(inode, attempt, "hydration wait timed out after 5 min, resetting");
                        let _ = db.set_status(inode, SyncStatus::Remote).await;
                        if attempt + 1 >= MAX_RETRIES {
                            return Err(SyncError::Transient(
                                format!("hydration timed out after {MAX_RETRIES} attempts"),
                            ));
                        }
                    }
                }
            }
        }
    }
    Err(SyncError::Transient("hydration failed after retries".into()))
}

async fn do_hydrate(
    db: &Arc<StateDb>, backend: &Arc<dyn Backend>, cache_dir: &PathBuf,
    waiters: &Arc<DashMap<Inode, Vec<oneshot::Sender<Result<(), libc::c_int>>>>>,
    entry: &FileEntry,
    base_store: &Arc<BaseStore>, sync_config: &Arc<SyncConfig>,
) -> Result<(), SyncError> {
    let cache_path = write_ops::safe_cache_path(cache_dir, &entry.remote_path)
        .map_err(|e| SyncError::Fatal(format!("unsafe cache path: errno {e}")))?;
    // Use inode + random suffix for temp files to prevent symlink races
    let rand: u64 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).unwrap_or_default()
        .subsec_nanos() as u64;
    let tmp_path   = cache_dir.join(".meta").join("partial")
                              .join(format!("{}.{:x}.tmp", entry.inode, rand));

    let result: Result<(), SyncError> = async {
        backend.download(&entry.remote_path, &tmp_path).await?;
        if let Some(p) = cache_path.parent() {
            tokio::fs::create_dir_all(p).await.map_err(SyncError::Io)?;
        }
        tokio::fs::rename(&tmp_path, &cache_path).await.map_err(SyncError::Io)?;
        let meta = tokio::fs::metadata(&cache_path).await.map_err(SyncError::Io)?;
        db.set_cached(entry.inode, &cache_path, meta.len(),
            entry.etag.as_deref(), entry.mtime, entry.size).await
            .map_err(|e| SyncError::Fatal(e.to_string()))?;

        // Snapshot base version for 3-way merge (best-effort, non-blocking)
        let max_size = sync_config.base_max_file_size_bytes().unwrap_or(10 * 1024 * 1024);
        let text_exts = sync_config.text_extensions.clone();
        if BaseStore::is_text_mergeable(&cache_path, meta.len(), max_size, &text_exts) {
            let bs = Arc::clone(base_store);
            let cp = cache_path.clone();
            let db2 = Arc::clone(db);
            let mount_id = entry.mount_id;
            let inode = entry.inode;
            tokio::task::spawn_blocking(move || {
                match bs.store_base(&cp) {
                    Ok(hash) => {
                        // Record the mapping in the DB (fire-and-forget via block_on)
                        let _ = tokio::runtime::Handle::current().block_on(
                            db2.set_base_hash(inode, mount_id, &hash, 0)
                        );
                        debug!(inode, %hash, "base version captured on hydration");
                    }
                    Err(e) => {
                        warn!(inode, "failed to capture base version: {e}");
                    }
                }
            });
        }

        debug!(inode = entry.inode, "hydrated");
        Ok(())
    }.await;

    if result.is_err() {
        // Roll back: reset status so a future open() retries hydration.
        // If this fails, the inode is stuck in Hydrating until daemon restart
        // (reset_hydrating handles that on startup).
        if let Err(e) = db.set_status(entry.inode, SyncStatus::Remote).await {
            warn!(inode = entry.inode, "failed to reset status after hydration error: {e}");
        }
        // Clean up partial download. ENOENT is expected if the download
        // failed before creating the file.
        if let Err(e) = tokio::fs::remove_file(&tmp_path).await {
            if e.kind() != std::io::ErrorKind::NotFound {
                warn!(inode = entry.inode, ?tmp_path, "failed to remove partial download: {e}");
            }
        }
    }
    if let Some((_, senders)) = waiters.remove(&entry.inode) {
        // Receiver dropped = waiter timed out or was cancelled; that's fine.
        for tx in senders { let _ = tx.send(result.as_ref().map(|_| ()).map_err(|_| libc::EIO)); }
    }
    result
}

async fn populate_directory(
    db: &Arc<StateDb>, backend: &Arc<dyn Backend>, mid: u32, dir: &FileEntry,
) -> Result<(), anyhow::Error> {
    debug!(inode = dir.inode, path = %dir.remote_path, "populating directory");
    let children = match backend.list(&dir.remote_path).await {
        Ok(c) => c,
        Err(e) if !dir.status.needs_hydration() => {
            // Directory exists locally (e.g. just created via mkdir) but the
            // remote listing failed — the async mkdir may not have completed
            // yet.  Treat as empty and mark listed so that create/lookup
            // inside it can proceed immediately.
            debug!(inode = dir.inode, error = %e, "locally-created dir not listable on remote — treating as empty");
            db.mark_dir_listed(dir.inode).await?;
            return Ok(());
        }
        Err(e) => return Err(anyhow::anyhow!("list {:?}: {e}", dir.remote_path)),
    };
    debug!(inode = dir.inode, count = children.len(), "listed children");

    // Load tombstones to skip recently-deleted entries
    let tombstones = db.active_tombstones(mid).await.unwrap_or_default();

    let entries: Vec<_> = children.iter().filter(|child| {
        // Reject entries with path traversal or null bytes
        if child.name.contains("..") || child.name.contains('\0') || child.name.contains('/') {
            warn!(name = %child.name, "skipping entry with unsafe filename");
            return false;
        }
        let full_path = write_ops::join_remote(&dir.remote_path, &child.name);
        // Skip tombstoned entries
        if tombstones.iter().any(|t| full_path == *t || full_path.starts_with(&format!("{t}/"))) {
            return false;
        }
        true
    }).map(|child| {
        let kind = if child.is_dir { FileKind::Directory } else { FileKind::File };
        let full_path = write_ops::join_remote(&dir.remote_path, &child.name);
        (child.name.clone(), full_path, kind, child.size, child.mtime, child.etag.clone())
    }).collect();

    db.batch_upsert_remote_files(mid, dir.inode, &entries).await
        .with_context(|| format!("batch upsert {} children under inode {}", entries.len(), dir.inode))?;

    debug!(inode = dir.inode, "upserts complete");
    db.mark_dir_listed(dir.inode).await?;
    debug!(inode = dir.inode, "directory populated");

    // Prefetch: populate child directories in the background so the next
    // cd/ls into a subdirectory is instant (one level of lookahead).
    spawn_prefetch_child_dirs(Arc::clone(db), Arc::clone(backend), mid, dir.inode);

    Ok(())
}

fn spawn_prefetch_child_dirs(
    db: Arc<StateDb>, backend: Arc<dyn Backend>, mid: u32, parent_inode: Inode,
) {
    tokio::spawn(async move {
        let children = db.list_children(mid, parent_inode).await.unwrap_or_default();
        let sem = Arc::new(tokio::sync::Semaphore::new(4));
        for child in children {
            if child.kind != FileKind::Directory || child.dir_listed.is_some() {
                continue;
            }
            let db = Arc::clone(&db);
            let backend = Arc::clone(&backend);
            let sem = Arc::clone(&sem);
            tokio::spawn(async move {
                let Ok(_permit) = sem.acquire().await else { return };
                // Inline the populate logic to avoid the !Send issue
                let list = match backend.list(&child.remote_path).await {
                    Ok(l) => l,
                    Err(e) => { debug!(inode = child.inode, "prefetch list failed: {e}"); return; }
                };
                let entries: Vec<_> = list.iter().filter(|c| {
                    !c.name.contains("..") && !c.name.contains('\0') && !c.name.contains('/')
                }).map(|c| {
                    let kind = if c.is_dir { FileKind::Directory } else { FileKind::File };
                    let full_path = write_ops::join_remote(&child.remote_path, &c.name);
                    (c.name.clone(), full_path, kind, c.size, c.mtime, c.etag.clone())
                }).collect();
                if let Err(e) = db.batch_upsert_remote_files(mid, child.inode, &entries).await {
                    debug!(inode = child.inode, "prefetch upsert failed: {e}");
                    return;
                }
                let _ = db.mark_dir_listed(child.inode).await;
                debug!(inode = child.inode, count = entries.len(), "prefetched directory");
            });
        }
    });
}

/// Background-prefetch small files after a directory listing.
/// Files under the configured `prefetch_threshold` are hydrated in the
/// background so they're cached before the user opens them.
fn spawn_prefetch_small_files(
    db: Arc<StateDb>, backend: Arc<dyn Backend>, mid: u32, parent_inode: Inode,
    cache_dir: PathBuf,
    waiters: Arc<DashMap<Inode, Vec<oneshot::Sender<Result<(), libc::c_int>>>>>,
    base_store: Arc<BaseStore>, sync_config: Arc<SyncConfig>,
) {
    let threshold = sync_config.prefetch_threshold_bytes();
    if threshold == 0 { return; }

    tokio::spawn(async move {
        let children = db.list_children(mid, parent_inode).await.unwrap_or_default();
        let sem = Arc::new(tokio::sync::Semaphore::new(2));

        for child in children {
            if child.kind != FileKind::File { continue; }
            if !child.status.needs_hydration() { continue; }
            if child.size > threshold { continue; }

            let db = Arc::clone(&db);
            let backend = Arc::clone(&backend);
            let cache_dir = cache_dir.clone();
            let waiters = Arc::clone(&waiters);
            let base_store = Arc::clone(&base_store);
            let sync_config = Arc::clone(&sync_config);
            let sem = Arc::clone(&sem);

            tokio::spawn(async move {
                let Ok(_permit) = sem.acquire().await else { return };
                if db.set_status(child.inode, SyncStatus::Hydrating).await.is_err() { return; }
                let _ = do_hydrate(
                    &db, &backend, &cache_dir, &waiters,
                    &child, &base_store, &sync_config,
                ).await;
            });
        }
    });
}

/// Background-prefetch the first N bytes of files in this directory
/// that are over `prefetch_threshold` (so the small-file pass won't
/// hydrate them in full). Without this, the first right-click on a
/// directory of large media files blocks the file-manager UI for a
/// few seconds per file while KIO sniffs MIME / builds thumbnails.
///
/// Skips any inode that already has a header on disk OR is currently
/// being fetched (tracked in `inflight`). The dedupe prevents the
/// runaway cat-storm we saw when Dolphin readdirs walked through every
/// dir in its tree-view + breadcrumb panel and each one re-spawned
/// prefetches for the same files.
fn spawn_prefetch_headers(
    db: Arc<StateDb>, backend: Arc<dyn Backend>, mid: u32, parent_inode: Inode,
    cache_dir: PathBuf, sync_config: Arc<SyncConfig>,
    inflight: Arc<DashMap<Inode, ()>>,
    sem: Arc<tokio::sync::Semaphore>,
    focus_dir: Arc<std::sync::atomic::AtomicU64>,
) {
    let header_size = sync_config.header_prefetch_size_bytes();
    let full_threshold = sync_config.prefetch_threshold_bytes();
    if header_size == 0 { return; }

    tokio::spawn(async move {
        let children = db.list_children(mid, parent_inode).await.unwrap_or_default();

        for child in children {
            if child.kind != FileKind::File { continue; }
            if !child.status.needs_hydration() { continue; }
            // The small-file pass hydrates everything <= full_threshold
            // in full, so a separate header pass is wasted work there.
            if child.size <= full_threshold { continue; }
            // Already cached on disk?
            if header_cache::header_path(&cache_dir, child.inode).exists() {
                continue;
            }
            // Already in flight (another readdir or read started a
            // fetch)? `insert` returns the previous value if the key
            // was already present, so Some means we lost the race.
            if inflight.insert(child.inode, ()).is_some() {
                continue;
            }

            let backend = Arc::clone(&backend);
            let cache_dir = cache_dir.clone();
            let sem = Arc::clone(&sem);
            let inflight = Arc::clone(&inflight);
            let inode = child.inode;
            let remote_path = child.remote_path.clone();
            let size = child.size;

            let focus_for_task = Arc::clone(&focus_dir);
            tokio::spawn(async move {
                let Ok(_permit) = sem.acquire().await else {
                    inflight.remove(&inode);
                    return;
                };
                // Bail if the user has moved on to a different directory
                // since we queued. Their current focus deserves the
                // bandwidth more than this stale dir.
                if focus_for_task.load(std::sync::atomic::Ordering::Relaxed) != parent_inode {
                    inflight.remove(&inode);
                    return;
                }
                // Re-check on disk after acquiring the permit — another
                // task may have completed while we were queued.
                if header_cache::header_path(&cache_dir, inode).exists() {
                    inflight.remove(&inode);
                    return;
                }
                let take = header_size.min(size);
                match backend.download_range(&remote_path, 0, take).await {
                    Ok(data) => {
                        if let Err(e) = header_cache::write_header(
                            &cache_dir, inode, &data,
                        ).await {
                            debug!(inode, "header write failed: {e}");
                        } else {
                            debug!(inode, len = data.len(), "header pre-fetched");
                        }
                    }
                    Err(e) => {
                        debug!(inode, "header pre-fetch failed: {e}");
                    }
                }
                inflight.remove(&inode);
            });
        }
    });
}

impl Filesystem for StratoFs {
    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let name_str = match name.to_str() { Some(s) => s.to_owned(), None => { reply.error(libc::EINVAL); return; } };
        let name_log = name_str.clone();
        let (db, backend, cfg, mid) = (Arc::clone(&self.db), Arc::clone(&self.backend), self.cfg.clone(), self.mount_id);
        let result = self.rt.block_on(async move {
            if let Some(e) = db.get_by_parent_name(mid, parent, &name_str).await? { return Ok(Some(e)); }
            if let Some(dir) = db.get_by_inode(parent).await? {
                if dir.kind == FileKind::Directory && dir.dir_listed.is_none() {
                    populate_directory(&db, &backend, mid, &dir).await?;
                    return db.get_by_parent_name(mid, parent, &name_str).await;
                }
            }
            Ok(None)
        });
        match result {
            Ok(Some(e)) => reply.entry(&cfg.entry_timeout(), &entry_to_attr(&e), 0),
            Ok(None)    => reply.error(libc::ENOENT),
            Err(e)      => { error!(parent, name = %name_log, "lookup: {e:#}"); reply.error(libc::EIO); }
        }
    }

    fn setattr(
        &mut self, _req: &Request<'_>, ino: u64, _mode: Option<u32>,
        _uid: Option<u32>, _gid: Option<u32>, size: Option<u64>,
        _atime: Option<fuser::TimeOrNow>, _mtime: Option<fuser::TimeOrNow>,
        _ctime: Option<SystemTime>, _fh: Option<u64>, _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>, _bkuptime: Option<SystemTime>,
        _flags: Option<u32>, reply: ReplyAttr,
    ) {
        let (db, cfg, cache_dir) = (Arc::clone(&self.db), self.cfg.clone(), self.cache_dir.clone());
        let queue = Arc::clone(&self.upload_queue);
        let result = self.rt.block_on(async {
            let entry = db.get_by_inode(ino).await?
                .ok_or_else(|| anyhow::anyhow!("inode {ino}"))?;

            // Handle truncate
            if let Some(new_size) = size {
                if let Some(cp) = &entry.cache_path {
                    let f = tokio::fs::OpenOptions::new().write(true).open(cp).await?;
                    f.set_len(new_size).await?;
                } else {
                    // File not hydrated — create a cache file at the right size
                    let cp = cache_dir.join(entry.remote_path.trim_start_matches('/'));
                    if let Some(p) = cp.parent() {
                        tokio::fs::create_dir_all(p).await?;
                    }
                    let f = tokio::fs::File::create(&cp).await?;
                    f.set_len(new_size).await?;
                }
                db.set_dirty_size(ino, new_size).await?;
                queue.enqueue(UploadTrigger::Write { inode: ino }).await;
            }

            // Re-read to return updated attrs
            db.get_by_inode(ino).await?
                .ok_or_else(|| anyhow::anyhow!("inode {ino} gone after setattr"))
        });
        match result {
            Ok(e) => reply.attr(&cfg.attr_timeout(), &entry_to_attr(&e)),
            Err(e) => { error!(ino, "setattr: {e}"); reply.error(libc::EIO); }
        }
    }

    fn getattr(&mut self, _req: &Request<'_>, ino: u64, reply: ReplyAttr) {
        let (db, cfg) = (Arc::clone(&self.db), self.cfg.clone());
        match self.rt.block_on(db.get_by_inode(ino)) {
            Ok(Some(e)) => reply.attr(&cfg.attr_timeout(), &entry_to_attr(&e)),
            Ok(None)    => reply.error(libc::ENOENT),
            Err(e)      => { error!("getattr: {e}"); reply.error(libc::EIO); }
        }
    }

    fn readdir(&mut self, _req: &Request<'_>, ino: u64, _fh: u64, offset: i64, mut reply: ReplyDirectory) {
        let (db, backend, mid) = (Arc::clone(&self.db), Arc::clone(&self.backend), self.mount_id);
        let (base_store, sync_config, cache_dir) = (
            Arc::clone(&self.base_store), Arc::clone(&self.sync_config), self.cache_dir.clone(),
        );
        let waiters = Arc::clone(&self.hydration_waiters);
        let inflight = Arc::clone(&self.prefetch_inflight);
        let prefetch_sem = Arc::clone(&self.prefetch_sem);
        let focus_dir = Arc::clone(&self.prefetch_focus_dir);
        // The current readdir target becomes "the dir the user cares
        // about right now". Older queued prefetches (for sidebar / tree
        // dirs the user moved past) will see the new value and bail
        // out before doing their rclone-cat.
        focus_dir.store(ino, std::sync::atomic::Ordering::Relaxed);
        let result = self.rt.block_on(async move {
            let dir = db.get_by_inode(ino).await?.ok_or_else(|| anyhow::anyhow!("inode {ino}"))?;
            if dir.dir_listed.is_none() {
                populate_directory(&db, &backend, mid, &dir).await?;
                // Prefetch small files in the background
                spawn_prefetch_small_files(
                    Arc::clone(&db), Arc::clone(&backend), mid, ino,
                    cache_dir.clone(), waiters, base_store, Arc::clone(&sync_config),
                );
            }
            // Trigger header prefetch on every readdir. Made safe by:
            //   - per-inode `inflight` dedup (DashMap),
            //   - on-disk header check before re-fetching,
            //   - shared `prefetch_sem` capping daemon-wide concurrency,
            //   - `prefetch_focus_dir` so stale queued tasks bail when
            //      the user moves on.
            spawn_prefetch_headers(
                Arc::clone(&db), Arc::clone(&backend), mid, ino,
                cache_dir, sync_config, inflight, prefetch_sem, focus_dir,
            );
            db.list_children(mid, ino).await
        });
        match result {
            Err(e) => { error!(ino, "readdir: {e:#}"); reply.error(libc::EIO); }
            Ok(children) => {
                // Hide rows whose (parent, name) collides — only the
                // lowest-inode row wins, matching what `get_by_parent_name`
                // (and therefore lookup) returns. Two children with the
                // same name in one directory is logically invalid; before
                // v0.12.1 the kernel readdir cache hid such phantoms, but
                // the post-mutation `inval_inode` introduced in v0.12.1
                // exposed them. Dedup keeps the visible filesystem state
                // consistent while we hunt the upstream cause.
                // `list_children` already sorts by inode ASC, so the first
                // occurrence of any name is the lowest-inode row.
                let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
                let unique: Vec<&FileEntry> = children.iter()
                    .filter(|e| seen.insert(e.name.as_str()))
                    .collect();
                let dropped = children.len() - unique.len();
                if dropped > 0 {
                    warn!(ino, dropped, "readdir hid duplicate (parent,name) rows");
                }
                debug!(ino, count = unique.len(), offset, "readdir returning entries");
                let dots: Vec<(u64, FileType, &str)> = vec![
                    (ino, FileType::Directory, "."),
                    (ino, FileType::Directory, ".."),
                ];
                let mut i = 0usize;
                for &(dino, ft, name) in &dots {
                    if i >= offset as usize && reply.add(dino, (i + 1) as i64, ft, name) {
                        reply.ok();
                        return;
                    }
                    i += 1;
                }
                for e in &unique {
                    let ft = match e.kind { FileKind::Directory => FileType::Directory, FileKind::Symlink => FileType::Symlink, _ => FileType::RegularFile };
                    if i >= offset as usize && reply.add(e.inode, (i + 1) as i64, ft, e.name.as_str()) {
                        reply.ok();
                        return;
                    }
                    i += 1;
                }
                reply.ok();
            }
        }
    }

    fn open(&mut self, _req: &Request<'_>, ino: u64, flags: i32, reply: ReplyOpen) {
        let (db, backend, cache_dir) = (Arc::clone(&self.db), Arc::clone(&self.backend), self.cache_dir.clone());
        let (open_files, next_fh, waiters) = (Arc::clone(&self.open_files), Arc::clone(&self.next_fh), Arc::clone(&self.hydration_waiters));
        let (base_store, sync_config) = (Arc::clone(&self.base_store), Arc::clone(&self.sync_config));
        let result = self.rt.block_on(async move {
            let entry = db.get_by_inode(ino).await?.ok_or_else(|| SyncError::NotFound(format!("{ino}")))?;

            let needs_hydration = entry.status.needs_hydration();
            let is_hydrating = entry.status.is_hydrating();
            let cache_path = entry.cache_path.clone().unwrap_or_else(|| {
                cache_dir.join(entry.remote_path.trim_start_matches('/'))
            });

            // Skip the auto-hydrate kick-off for huge files: a
            // file-manager open()/read(0,32K) probe should never trigger
            // a multi-GB background download. Reads past the header
            // cache will fall back to the existing range-download path,
            // and the user can `pin` a file (or set
            // `auto_hydrate_max_size = "0"`) to force full hydration.
            let auto_hydrate = match sync_config.auto_hydrate_max_size_bytes() {
                Some(cap) => entry.size <= cap,
                None      => true,
            };
            if needs_hydration && auto_hydrate {
                // Start download in background — don't block open()
                db.set_status(ino, SyncStatus::Hydrating).await
                    .map_err(|e| SyncError::Fatal(e.to_string()))?;
                let (db2, be2, cd2, w2) = (
                    Arc::clone(&db), Arc::clone(&backend), cache_dir.clone(), Arc::clone(&waiters),
                );
                let (bs2, sc2) = (Arc::clone(&base_store), Arc::clone(&sync_config));
                tokio::spawn(async move {
                    let entry = match db2.get_by_inode(ino).await {
                        Ok(Some(e)) => e,
                        _ => return,
                    };
                    let _ = do_hydrate(&db2, &be2, &cd2, &w2, &entry, &bs2, &sc2).await;
                });
            } else if !is_hydrating {
                // Already cached/dirty — touch LRU
                if let Err(e) = db.touch_lru(ino).await {
                    warn!(ino, "touch_lru failed: {e}");
                }
            }

            let fh = next_fh.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            open_files.insert(fh, OpenFile {
                inode: ino,
                cache_path,
                remote_path: entry.remote_path.clone(),
                flags,
                hydrating: needs_hydration || is_hydrating,
            });
            Ok::<u64, SyncError>(fh)
        });
        match result { Ok(fh) => reply.opened(fh, 0), Err(e) => { error!(ino, "open: {e}"); reply.error(errno(&e)); } }
    }

    fn read(&mut self, _req: &Request<'_>, _ino: u64, fh: u64, offset: i64, size: u32, _flags: i32, _lock: Option<u64>, reply: ReplyData) {
        // Async-spawn instead of block_on. The fuser session loop reads
        // requests serially on a single thread; if read() blocks until
        // the work completes, every other FUSE request — including N-1
        // sibling reads from the same right-click — queues behind it.
        // Spawning frees the FUSE worker thread immediately so all 11
        // concurrent sniff reads run in parallel through the rclone
        // backend instead of serially. Reply is sent from inside the
        // task once the work completes; ReplyData is Send.
        let open_files = Arc::clone(&self.open_files);
        let (db, backend, cache_dir, waiters) = (
            Arc::clone(&self.db), Arc::clone(&self.backend),
            self.cache_dir.clone(), Arc::clone(&self.hydration_waiters),
        );
        let (base_store, sync_config) = (Arc::clone(&self.base_store), Arc::clone(&self.sync_config));
        let prefetch_inflight = Arc::clone(&self.prefetch_inflight);
        self.rt.spawn(async move {
            let result: Result<Vec<u8>, SyncError> = async {
            let (ino, needs_wait, remote_path) = {
                let entry = open_files.get(&fh).ok_or_else(|| SyncError::Fatal(format!("bad fh {fh}")))?;
                (entry.inode, entry.hydrating, entry.remote_path.clone())
            };

            if needs_wait {
                // Header cache fast path: file managers (Dolphin / KIO,
                // Nautilus + thumbnailers) open every selected file and
                // read 16-64 KiB from offset 0 to sniff MIME / generate
                // thumbnails. Without this the right-click context menu
                // blocks for ~3 s × N on rclone-cat round trips. Misses
                // fall through to the existing range-download path.
                let header_size = sync_config.header_prefetch_size_bytes();
                let off = offset.max(0) as u64;
                let len = size as u64;
                let read_within_header = header_size > 0
                    && off.saturating_add(len) <= header_size;
                if read_within_header {
                    match header_cache::read_header(&cache_dir, ino, off, len).await {
                        Ok(Some(data)) => return Ok(data),
                        Ok(None) => { /* miss — fall through */ }
                        Err(e) => {
                            debug!(ino, "header cache read failed, falling through: {e}");
                        }
                    }
                }

                // Check if full hydration has completed since open()
                if let Ok(Some(fe)) = db.get_by_inode(ino).await {
                    if fe.status.has_local_data() && !fe.status.is_hydrating() {
                        // Full download finished — read from cache
                        if let Some(mut entry) = open_files.get_mut(&fh) {
                            if let Some(ref cp) = fe.cache_path { entry.cache_path = cp.clone(); }
                            entry.hydrating = false;
                        }
                        let _ = db.touch_lru(ino).await;
                        // Fall through to normal cache read below
                    } else {
                        // If the read is past the header range AND the
                        // file is still Remote (open()-time auto-hydrate
                        // was deferred), this is the user actually
                        // accessing content, not a MIME / thumbnail
                        // sniff — kick off full hydration now. Reads
                        // *within* the header range never trigger this:
                        // they're satisfied by a one-shot range download
                        // and (below) cached for next time.
                        if !read_within_header && fe.status == SyncStatus::Remote {
                            if db.set_status(ino, SyncStatus::Hydrating).await.is_ok() {
                                let (db2, be2, cd2, w2) = (
                                    Arc::clone(&db), Arc::clone(&backend),
                                    cache_dir.clone(), Arc::clone(&waiters),
                                );
                                let (bs2, sc2) = (Arc::clone(&base_store), Arc::clone(&sync_config));
                                tokio::spawn(async move {
                                    let entry = match db2.get_by_inode(ino).await {
                                        Ok(Some(e)) => e,
                                        _ => return,
                                    };
                                    let _ = do_hydrate(&db2, &be2, &cd2, &w2, &entry, &bs2, &sc2).await;
                                });
                            }
                        }

                        // Still hydrating — try range download for this read
                        match backend.download_range(&remote_path, offset as u64, size as u64).await {
                            Ok(data) => {
                                // Opportunistic: cache whatever bytes we
                                // just got so the next sniff at offset 0
                                // hits the disk instead of the network.
                                // We deliberately DON'T issue an extra
                                // fetch to fill out to `header_size` — a
                                // partial header still serves any sniff
                                // that asks for ≤ what we cached, and
                                // the previous "fetch the rest" version
                                // doubled bandwidth on every cold sniff.
                                // Dedupe via the inflight set so 11
                                // simultaneous KIO sniffs on the same
                                // file don't all spawn writes.
                                if read_within_header
                                    && off == 0
                                    && header_size > 0
                                    && !data.is_empty()
                                    && !header_cache::header_path(&cache_dir, ino).exists()
                                    && prefetch_inflight.insert(ino, ()).is_none()
                                {
                                    let cd2 = cache_dir.clone();
                                    let inflight2 = Arc::clone(&prefetch_inflight);
                                    let bytes = data.clone();
                                    tokio::spawn(async move {
                                        if let Err(e) = header_cache::write_header(
                                            &cd2, ino, &bytes,
                                        ).await {
                                            debug!(inode = ino, "lazy header write failed: {e}");
                                        } else {
                                            debug!(inode = ino, len = bytes.len(),
                                                "lazy header captured from sniff read");
                                        }
                                        inflight2.remove(&ino);
                                    });
                                }
                                return Ok(data);
                            }
                            Err(SyncError::NotSupported) => {
                                // Backend doesn't support ranges — fall back to blocking wait
                                hydrate_if_needed(&db, &backend, &cache_dir, ino, &waiters, &base_store, &sync_config).await?;
                                if let Some(mut entry) = open_files.get_mut(&fh) {
                                    if let Ok(Some(fe)) = db.get_by_inode(entry.inode).await {
                                        if let Some(cp) = fe.cache_path { entry.cache_path = cp; }
                                    }
                                    entry.hydrating = false;
                                }
                                let _ = db.touch_lru(ino).await;
                            }
                            Err(e) => {
                                // Range download failed — fall back to blocking wait
                                warn!(ino, "range download failed, waiting for full hydration: {e}");
                                hydrate_if_needed(&db, &backend, &cache_dir, ino, &waiters, &base_store, &sync_config).await?;
                                if let Some(mut entry) = open_files.get_mut(&fh) {
                                    if let Ok(Some(fe)) = db.get_by_inode(entry.inode).await {
                                        if let Some(cp) = fe.cache_path { entry.cache_path = cp; }
                                    }
                                    entry.hydrating = false;
                                }
                                let _ = db.touch_lru(ino).await;
                            }
                        }
                    }
                }
            }

            // Normal read from cache file
            let entry = open_files.get(&fh).ok_or_else(|| SyncError::Fatal(format!("bad fh {fh}")))?;
            use tokio::io::{AsyncReadExt, AsyncSeekExt};
            let mut f = tokio::fs::File::open(&entry.cache_path).await.map_err(SyncError::Io)?;
            f.seek(std::io::SeekFrom::Start(offset as u64)).await.map_err(SyncError::Io)?;
            let mut buf = vec![0u8; size as usize];
            let n = f.read(&mut buf).await.map_err(SyncError::Io)?;
            buf.truncate(n);
            Ok::<Vec<u8>, SyncError>(buf)
            }.await;
            match result {
                Ok(data) => reply.data(&data),
                Err(e)   => { error!(fh, "read: {e}"); reply.error(errno(&e)); }
            }
        });
    }

    fn write(&mut self, _req: &Request<'_>, _ino: u64, fh: u64, offset: i64, data: &[u8], _wf: u32, _flags: i32, _lock: Option<u64>, reply: ReplyWrite) {
        let (open_files, db, queue, data) = (Arc::clone(&self.open_files), Arc::clone(&self.db), Arc::clone(&self.upload_queue), data.to_vec());
        match self.rt.block_on(write_ops::handle_write(fh, offset, &data, &open_files, &db, &queue)) {
            Ok(n) => reply.written(n), Err(e) => reply.error(e),
        }
    }

    fn release(&mut self, _req: &Request<'_>, _ino: u64, fh: u64, _flags: i32, _lock: Option<u64>, _flush: bool, reply: ReplyEmpty) {
        let (open_files, db, queue) = (Arc::clone(&self.open_files), Arc::clone(&self.db), Arc::clone(&self.upload_queue));
        self.rt.block_on(write_ops::handle_release(fh, &open_files, &db, &queue));
        reply.ok();
    }

    fn fsync(&mut self, _req: &Request<'_>, _ino: u64, fh: u64, _datasync: bool, reply: ReplyEmpty) {
        let (open_files, queue) = (Arc::clone(&self.open_files), Arc::clone(&self.upload_queue));
        match self.rt.block_on(write_ops::handle_fsync(fh, &open_files, &queue)) { Ok(()) => reply.ok(), Err(e) => reply.error(e), }
    }

    fn create(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, _mode: u32, _umask: u32, _flags: i32, reply: ReplyCreate) {
        let name_str = match name.to_str() { Some(s) => s.to_owned(), None => { reply.error(libc::EINVAL); return; } };
        let (db, cache_dir, open_files, next_fh, cfg, mid, ignore) = (Arc::clone(&self.db), self.cache_dir.clone(), Arc::clone(&self.open_files), Arc::clone(&self.next_fh), self.cfg.clone(), self.mount_id, Arc::clone(&self.ignore));
        match self.rt.block_on(write_ops::handle_create(parent, &name_str, mid, &db, &cache_dir, &open_files, &next_fh, &ignore)) {
            Ok((inode, fh)) => {
                let attr = FileAttr { ino: inode, size: 0, blocks: 0, atime: SystemTime::now(), mtime: SystemTime::now(), ctime: SystemTime::now(), crtime: SystemTime::now(), kind: FileType::RegularFile, perm: 0o644, nlink: 1, uid: unsafe { libc::getuid() }, gid: unsafe { libc::getgid() }, rdev: 0, blksize: 4096, flags: 0 };
                self.invalidate_dir_cache(parent);
                reply.created(&cfg.entry_timeout(), &attr, 0, fh, 0);
            }
            Err(e) => reply.error(e),
        }
    }

    fn mkdir(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, _mode: u32, _umask: u32, reply: ReplyEntry) {
        let name_str = match name.to_str() { Some(s) => s.to_owned(), None => { reply.error(libc::EINVAL); return; } };
        let (db, backend, cfg, mid, ignore) = (Arc::clone(&self.db), Arc::clone(&self.backend), self.cfg.clone(), self.mount_id, Arc::clone(&self.ignore));
        match self.rt.block_on(write_ops::handle_mkdir(parent, &name_str, mid, &db, &backend, &ignore)) {
            Ok(inode) => {
                let attr = FileAttr { ino: inode, size: 0, blocks: 0, atime: SystemTime::now(), mtime: SystemTime::now(), ctime: SystemTime::now(), crtime: SystemTime::now(), kind: FileType::Directory, perm: 0o755, nlink: 2, uid: unsafe { libc::getuid() }, gid: unsafe { libc::getgid() }, rdev: 0, blksize: 4096, flags: 0 };
                self.invalidate_dir_cache(parent);
                reply.entry(&cfg.entry_timeout(), &attr, 0);
            }
            Err(e) => reply.error(e),
        }
    }

    fn unlink(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let name_str = match name.to_str() { Some(s) => s.to_owned(), None => { reply.error(libc::EINVAL); return; } };
        let (db, backend, mid, cache_dir) = (
            Arc::clone(&self.db), Arc::clone(&self.backend),
            self.mount_id, self.cache_dir.clone(),
        );
        match self.rt.block_on(write_ops::handle_unlink(parent, &name_str, mid, &db, &backend, &cache_dir)) {
            Ok(()) => { self.invalidate_dir_cache(parent); reply.ok(); }
            Err(e) => reply.error(e),
        }
    }

    fn rmdir(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let name_str = match name.to_str() { Some(s) => s.to_owned(), None => { reply.error(libc::EINVAL); return; } };
        let (db, backend, mid) = (Arc::clone(&self.db), Arc::clone(&self.backend), self.mount_id);
        match self.rt.block_on(write_ops::handle_rmdir(parent, &name_str, mid, &db, &backend)) {
            Ok(()) => { self.invalidate_dir_cache(parent); reply.ok(); }
            Err(e) => reply.error(e),
        }
    }

    fn rename(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, new_parent: u64, new_name: &OsStr, _flags: u32, reply: ReplyEmpty) {
        let (n, nn) = match (name.to_str(), new_name.to_str()) { (Some(a), Some(b)) => (a.to_owned(), b.to_owned()), _ => { reply.error(libc::EINVAL); return; } };
        let (db, backend, mid, cache_dir) = (
            Arc::clone(&self.db), Arc::clone(&self.backend),
            self.mount_id, self.cache_dir.clone(),
        );
        match self.rt.block_on(write_ops::handle_rename(parent, &n, new_parent, &nn, mid, &db, &backend, &cache_dir)) {
            Ok(()) => {
                self.invalidate_dir_cache(parent);
                if new_parent != parent { self.invalidate_dir_cache(new_parent); }
                reply.ok();
            }
            Err(e) => reply.error(e),
        }
    }

    fn getxattr(&mut self, _req: &Request<'_>, ino: u64, name: &OsStr, size: u32, reply: ReplyXattr) {
        let name_bytes = name.as_bytes();
        let db = Arc::clone(&self.db);

        let entry = match self.rt.block_on(db.get_by_inode(ino)) {
            Ok(Some(e)) => e,
            Ok(None)    => { reply.error(libc::ENOENT); return; }
            Err(reason) => { error!(ino, "getxattr db lookup: {reason}"); reply.error(libc::EIO); return; }
        };

        let value: &[u8] = if name_bytes == XATTR_STATUS {
            entry.status.as_str().as_bytes()
        } else if name_bytes == XATTR_ETAG {
            entry.etag.as_deref().unwrap_or("").as_bytes()
        } else if name_bytes == XATTR_REMOTE_PATH {
            entry.remote_path.as_bytes()
        } else {
            reply.error(libc::ENODATA);
            return;
        };

        if size == 0 {
            reply.size(value.len() as u32);
        } else if size >= value.len() as u32 {
            reply.data(value);
        } else {
            reply.error(libc::ERANGE);
        }
    }

    fn listxattr(&mut self, _req: &Request<'_>, ino: u64, size: u32, reply: ReplyXattr) {
        let db = Arc::clone(&self.db);
        match self.rt.block_on(db.get_by_inode(ino)) {
            Ok(Some(_)) => {}
            Ok(None)    => { reply.error(libc::ENOENT); return; }
            Err(reason) => { error!(ino, "listxattr db lookup: {reason}"); reply.error(libc::EIO); return; }
        };

        // xattr list format: null-terminated names concatenated
        let mut buf = Vec::new();
        for attr in [XATTR_STATUS, XATTR_ETAG, XATTR_REMOTE_PATH] {
            buf.extend_from_slice(attr);
            buf.push(0);
        }

        if size == 0 {
            reply.size(buf.len() as u32);
        } else if size >= buf.len() as u32 {
            reply.data(&buf);
        } else {
            reply.error(libc::ERANGE);
        }
    }

    fn statfs(&mut self, _req: &Request<'_>, _ino: u64, reply: ReplyStatfs) {
        match host_statfs(&self.cache_dir) {
            Ok(t) => reply.statfs(
                t.blocks, t.bfree, t.bavail,
                t.files, t.ffree,
                t.bsize, t.namelen, t.frsize,
            ),
            Err(e) => {
                warn!(cache_dir = ?self.cache_dir, "statfs failed: {e}");
                reply.error(e.raw_os_error().unwrap_or(libc::EIO));
            }
        }
    }

    fn setxattr(&mut self, _req: &Request<'_>, _ino: u64, _name: &OsStr, _value: &[u8], _flags: i32, _position: u32, reply: ReplyEmpty) {
        reply.error(libc::ENOTSUP);
    }

    fn removexattr(&mut self, _req: &Request<'_>, _ino: u64, _name: &OsStr, reply: ReplyEmpty) {
        reply.error(libc::ENOTSUP);
    }
}

/// Ensure inode 1 is the mount root directory. Must be called before the
/// poller or FUSE thread start (poller inserts with parent=FUSE_ROOT_INODE,
/// so the FK requires inode 1 to exist). Also validates that any existing
/// inode 1 is actually the root directory — a stale DB from a broken run
/// could have a non-directory entry there.
pub async fn ensure_root(db: &Arc<StateDb>, mount_id: u32) -> anyhow::Result<()> {
    let needs_root = match db.get_by_inode(FUSE_ROOT_INODE).await? {
        None => true,
        Some(entry) => {
            if entry.kind != FileKind::Directory
                || entry.remote_path != "/"
                || entry.mount_id != mount_id
            {
                warn!(
                    inode = FUSE_ROOT_INODE,
                    kind = ?entry.kind,
                    remote_path = %entry.remote_path,
                    "root inode is corrupt — clearing stale state"
                );
                db.delete_mount_entries(mount_id).await?;
                true
            } else {
                false
            }
        }
    };
    if needs_root {
        db.insert_root(&NewFileEntry {
            mount_id,
            parent: 0, // ignored by insert_root — uses NULL for parent_inode
            name: "/".into(),
            remote_path: "/".into(),
            kind: FileKind::Directory,
            size: 0,
            mtime: SystemTime::now(),
            etag: None,
            status: SyncStatus::Remote,
            cache_path: None,
            cache_size: None,
        }).await?;
    }
    Ok(())
}

pub fn mount(
    mount_name: &str, mount_id: u32, mount_path: &std::path::Path,
    cache_dir: PathBuf, db: Arc<StateDb>, backend: Arc<dyn Backend>,
    upload_queue: Arc<UploadQueue>,
    base_store: Arc<BaseStore>, sync_config: Arc<SyncConfig>,
    cfg: FuseConfig, rt: Handle,
    hydration_waiters: Arc<DashMap<Inode, Vec<oneshot::Sender<Result<(), libc::c_int>>>>>,
    ignore: Arc<GlobSet>,
) -> anyhow::Result<()> {
    use fuser::MountOption;
    use std::os::unix::fs::PermissionsExt;
    let partial_dir = cache_dir.join(".meta").join("partial");
    std::fs::create_dir_all(&partial_dir)?;
    // Restrict partial dir to owner-only (prevents symlink attacks by other users)
    std::fs::set_permissions(&partial_dir, std::fs::Permissions::from_mode(0o700))?;
    std::fs::create_dir_all(mount_path)?;
    let notifier_slot = Arc::new(OnceLock::new());
    let fs = StratoFs {
        mount_id, mount_name: mount_name.to_owned(), db, backend, base_store,
        sync_config, cache_dir, cfg: cfg.clone(), rt,
        open_files: Arc::new(DashMap::new()),
        next_fh: Arc::new(AtomicU64::new(1)),
        hydration_waiters, upload_queue, ignore,
        notifier: Arc::clone(&notifier_slot),
        prefetch_inflight: Arc::new(DashMap::new()),
        // Header prefetch concurrency. Each rclone-cat call is dominated
        // by process spawn + OAuth refresh + cloud round-trip (~2-3 s),
        // and the per-call CPU cost is negligible — running many in
        // parallel is mostly bound by rclone process count and Drive
        // API quota. 16 hits a sweet spot: fills a typical media folder
        // (10-20 large files) in 2-3 s once focus lands on it, without
        // forking dozens of concurrent rclone processes.
        prefetch_sem:      Arc::new(tokio::sync::Semaphore::new(16)),
        prefetch_focus_dir: Arc::new(std::sync::atomic::AtomicU64::new(0)),
    };
    let mut opts = vec![MountOption::FSName(format!("stratosync:{mount_name}")), MountOption::AutoUnmount, MountOption::DefaultPermissions];
    if cfg.allow_other { opts.push(MountOption::AllowOther); }
    // Use Session directly (rather than fuser::mount2) so we can pull a
    // Notifier off it and give it to the filesystem. Without this, the
    // kernel's readdir cache lingers after directory mutations and KIO/
    // Dolphin won't show new entries until the user re-enters the folder.
    let mut session = fuser::Session::new(fs, mount_path, &opts)?;
    let _ = notifier_slot.set(session.notifier());
    session.run()?;
    Ok(())
}

#[cfg(test)]
mod statfs_tests {
    use super::host_statfs;
    use std::path::Path;

    #[test]
    fn host_statfs_returns_realistic_values_for_real_path() {
        // Without this implementation, fuser's default statfs returns
        // zeros and Dolphin refuses to paste with "not enough room on
        // the device." Sanity-check that we now return real numbers.
        let stats = host_statfs(Path::new("/tmp")).expect("statvfs /tmp must succeed");
        assert!(stats.blocks > 0, "blocks must be > 0 (got {})", stats.blocks);
        assert!(stats.bsize >= 512, "bsize must be sane (got {})", stats.bsize);
        assert!(stats.namelen > 0, "namelen must be > 0");
        assert!(stats.frsize > 0, "frsize must be > 0");
    }

    #[test]
    fn host_statfs_propagates_enoent_for_missing_path() {
        let err = host_statfs(Path::new("/definitely/does/not/exist/xyzzy"))
            .expect_err("statvfs on a missing path must error");
        assert_eq!(err.raw_os_error(), Some(libc::ENOENT));
    }
}
