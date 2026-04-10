#![allow(unused_imports, unused_variables, dead_code)]
/// FUSE filesystem implementation — Phase 1 (read) + Phase 2 (write).
/// See docs/architecture/02-fuse-layer.md for design details.
pub mod write_ops;

use std::ffi::OsStr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::SystemTime;

use anyhow::Context;
use dashmap::DashMap;
use fuser::{
    FileAttr, FileType, Filesystem, ReplyAttr, ReplyCreate, ReplyData,
    ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyWrite, Request,
};
use tokio::runtime::Handle;
use tokio::sync::oneshot;
#[allow(unused_imports)]
use tracing::{debug, error, warn};

use stratosync_core::{
    backend::Backend, config::FuseConfig, state::{NewFileEntry, StateDb}, types::*,
};
use crate::sync::upload_queue::{UploadQueue, UploadTrigger};

pub struct OpenFile {
    pub inode:      Inode,
    pub cache_path: PathBuf,
    pub flags:      i32,
    pub hydrating:  bool,
}

pub struct StratoFs {
    pub mount_id:          u32,
    pub mount_name:        String,
    pub db:                Arc<StateDb>,
    pub backend:           Arc<dyn Backend>,
    pub cache_dir:         PathBuf,
    pub cfg:               FuseConfig,
    pub rt:                Handle,
    pub open_files:        Arc<DashMap<u64, OpenFile>>,
    pub next_fh:           Arc<AtomicU64>,
    pub hydration_waiters: Arc<DashMap<Inode, Vec<oneshot::Sender<Result<(), libc::c_int>>>>>,
    pub upload_queue:      Arc<UploadQueue>,
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

fn errno(e: &SyncError) -> libc::c_int {
    match e {
        SyncError::NotFound(_)         => libc::ENOENT,
        SyncError::PermissionDenied(_) => libc::EACCES,
        SyncError::QuotaExceeded       => libc::ENOSPC,
        SyncError::Network(_)          => libc::EHOSTUNREACH,
        SyncError::Transient(_)        => libc::EAGAIN,
        SyncError::Conflict { .. }     => libc::EEXIST,
        SyncError::Io(io)              => io.raw_os_error().unwrap_or(libc::EIO),
        _                              => libc::EIO,
    }
}

pub async fn hydrate_if_needed(
    db: &Arc<StateDb>, backend: &Arc<dyn Backend>, cache_dir: &PathBuf,
    inode: Inode,
    waiters: &Arc<DashMap<Inode, Vec<oneshot::Sender<Result<(), libc::c_int>>>>>,
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
                return do_hydrate(db, backend, cache_dir, waiters, &entry).await;
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
        let result = self.rt.block_on(async move {
            let dir = db.get_by_inode(ino).await?.ok_or_else(|| anyhow::anyhow!("inode {ino}"))?;
            if dir.dir_listed.is_none() { populate_directory(&db, &backend, mid, &dir).await?; }
            db.list_children(mid, ino).await
        });
        match result {
            Err(e) => { error!(ino, "readdir: {e:#}"); reply.error(libc::EIO); }
            Ok(children) => {
                debug!(ino, count = children.len(), offset, "readdir returning entries");
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
                for e in &children {
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
        let result = self.rt.block_on(async move {
            let entry = db.get_by_inode(ino).await?.ok_or_else(|| SyncError::NotFound(format!("{ino}")))?;

            let needs_hydration = entry.status.needs_hydration();
            let is_hydrating = entry.status.is_hydrating();
            let cache_path = entry.cache_path.clone().unwrap_or_else(|| {
                cache_dir.join(entry.remote_path.trim_start_matches('/'))
            });

            if needs_hydration {
                // Start download in background — don't block open()
                db.set_status(ino, SyncStatus::Hydrating).await
                    .map_err(|e| SyncError::Fatal(e.to_string()))?;
                let (db2, be2, cd2, w2) = (
                    Arc::clone(&db), Arc::clone(&backend), cache_dir.clone(), Arc::clone(&waiters),
                );
                tokio::spawn(async move {
                    let entry = match db2.get_by_inode(ino).await {
                        Ok(Some(e)) => e,
                        _ => return,
                    };
                    let _ = do_hydrate(&db2, &be2, &cd2, &w2, &entry).await;
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
                flags,
                hydrating: needs_hydration || is_hydrating,
            });
            Ok::<u64, SyncError>(fh)
        });
        match result { Ok(fh) => reply.opened(fh, 0), Err(e) => { error!(ino, "open: {e}"); reply.error(errno(&e)); } }
    }

    fn read(&mut self, _req: &Request<'_>, _ino: u64, fh: u64, offset: i64, size: u32, _flags: i32, _lock: Option<u64>, reply: ReplyData) {
        let open_files = Arc::clone(&self.open_files);
        let (db, backend, cache_dir, waiters) = (
            Arc::clone(&self.db), Arc::clone(&self.backend),
            self.cache_dir.clone(), Arc::clone(&self.hydration_waiters),
        );
        let result = self.rt.block_on(async move {
            // If file is still hydrating, wait for download to complete
            let (ino, needs_wait) = {
                let entry = open_files.get(&fh).ok_or_else(|| SyncError::Fatal(format!("bad fh {fh}")))?;
                (entry.inode, entry.hydrating)
            };
            if needs_wait {
                hydrate_if_needed(&db, &backend, &cache_dir, ino, &waiters).await?;
                // Update the OpenFile with the actual cache path
                if let Some(mut entry) = open_files.get_mut(&fh) {
                    if let Ok(Some(fe)) = db.get_by_inode(entry.inode).await {
                        if let Some(cp) = fe.cache_path {
                            entry.cache_path = cp;
                        }
                    }
                    entry.hydrating = false;
                }
                if let Err(e) = db.touch_lru(ino).await {
                    warn!(ino, "touch_lru failed: {e}");
                }
            }

            let entry = open_files.get(&fh).ok_or_else(|| SyncError::Fatal(format!("bad fh {fh}")))?;
            use tokio::io::{AsyncReadExt, AsyncSeekExt};
            let mut f = tokio::fs::File::open(&entry.cache_path).await.map_err(SyncError::Io)?;
            f.seek(std::io::SeekFrom::Start(offset as u64)).await.map_err(SyncError::Io)?;
            let mut buf = vec![0u8; size as usize];
            let n = f.read(&mut buf).await.map_err(SyncError::Io)?;
            buf.truncate(n);
            Ok::<Vec<u8>, SyncError>(buf)
        });
        match result { Ok(data) => reply.data(&data), Err(e) => { error!(fh, "read: {e}"); reply.error(errno(&e)); } }
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
        let (db, cache_dir, open_files, next_fh, cfg, mid) = (Arc::clone(&self.db), self.cache_dir.clone(), Arc::clone(&self.open_files), Arc::clone(&self.next_fh), self.cfg.clone(), self.mount_id);
        match self.rt.block_on(write_ops::handle_create(parent, &name_str, mid, &db, &cache_dir, &open_files, &next_fh)) {
            Ok((inode, fh)) => {
                let attr = FileAttr { ino: inode, size: 0, blocks: 0, atime: SystemTime::now(), mtime: SystemTime::now(), ctime: SystemTime::now(), crtime: SystemTime::now(), kind: FileType::RegularFile, perm: 0o644, nlink: 1, uid: unsafe { libc::getuid() }, gid: unsafe { libc::getgid() }, rdev: 0, blksize: 4096, flags: 0 };
                reply.created(&cfg.entry_timeout(), &attr, 0, fh, 0);
            }
            Err(e) => reply.error(e),
        }
    }

    fn mkdir(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, _mode: u32, _umask: u32, reply: ReplyEntry) {
        let name_str = match name.to_str() { Some(s) => s.to_owned(), None => { reply.error(libc::EINVAL); return; } };
        let (db, backend, cfg, mid) = (Arc::clone(&self.db), Arc::clone(&self.backend), self.cfg.clone(), self.mount_id);
        match self.rt.block_on(write_ops::handle_mkdir(parent, &name_str, mid, &db, &backend)) {
            Ok(inode) => {
                let attr = FileAttr { ino: inode, size: 0, blocks: 0, atime: SystemTime::now(), mtime: SystemTime::now(), ctime: SystemTime::now(), crtime: SystemTime::now(), kind: FileType::Directory, perm: 0o755, nlink: 2, uid: unsafe { libc::getuid() }, gid: unsafe { libc::getgid() }, rdev: 0, blksize: 4096, flags: 0 };
                reply.entry(&cfg.entry_timeout(), &attr, 0);
            }
            Err(e) => reply.error(e),
        }
    }

    fn unlink(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let name_str = match name.to_str() { Some(s) => s.to_owned(), None => { reply.error(libc::EINVAL); return; } };
        let (db, backend, mid) = (Arc::clone(&self.db), Arc::clone(&self.backend), self.mount_id);
        match self.rt.block_on(write_ops::handle_unlink(parent, &name_str, mid, &db, &backend)) { Ok(()) => reply.ok(), Err(e) => reply.error(e), }
    }

    fn rmdir(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let name_str = match name.to_str() { Some(s) => s.to_owned(), None => { reply.error(libc::EINVAL); return; } };
        let (db, backend, mid) = (Arc::clone(&self.db), Arc::clone(&self.backend), self.mount_id);
        match self.rt.block_on(write_ops::handle_rmdir(parent, &name_str, mid, &db, &backend)) { Ok(()) => reply.ok(), Err(e) => reply.error(e), }
    }

    fn rename(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, new_parent: u64, new_name: &OsStr, _flags: u32, reply: ReplyEmpty) {
        let (n, nn) = match (name.to_str(), new_name.to_str()) { (Some(a), Some(b)) => (a.to_owned(), b.to_owned()), _ => { reply.error(libc::EINVAL); return; } };
        let (db, backend, mid) = (Arc::clone(&self.db), Arc::clone(&self.backend), self.mount_id);
        match self.rt.block_on(write_ops::handle_rename(parent, &n, new_parent, &nn, mid, &db, &backend)) { Ok(()) => reply.ok(), Err(e) => reply.error(e), }
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
    upload_queue: Arc<UploadQueue>, cfg: FuseConfig, rt: Handle,
) -> anyhow::Result<()> {
    use fuser::MountOption;
    use std::os::unix::fs::PermissionsExt;
    let partial_dir = cache_dir.join(".meta").join("partial");
    std::fs::create_dir_all(&partial_dir)?;
    // Restrict partial dir to owner-only (prevents symlink attacks by other users)
    std::fs::set_permissions(&partial_dir, std::fs::Permissions::from_mode(0o700))?;
    std::fs::create_dir_all(mount_path)?;
    let fs = StratoFs { mount_id, mount_name: mount_name.to_owned(), db, backend, cache_dir, cfg: cfg.clone(), rt, open_files: Arc::new(DashMap::new()), next_fh: Arc::new(AtomicU64::new(1)), hydration_waiters: Arc::new(DashMap::new()), upload_queue };
    let mut opts = vec![MountOption::FSName(format!("stratosync:{mount_name}")), MountOption::AutoUnmount, MountOption::DefaultPermissions];
    if cfg.allow_other { opts.push(MountOption::AllowOther); }
    fuser::mount2(fs, mount_path, &opts)?;
    Ok(())
}
