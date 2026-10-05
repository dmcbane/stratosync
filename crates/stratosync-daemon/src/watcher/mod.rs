/// inotify watcher — watches the cache directory for local modifications
/// and feeds them into the UploadQueue.
///
/// We watch the local cache directory rather than the FUSE mount point
/// because watching a FUSE mount creates a feedback loop (our own reads
/// trigger events).  The cache dir reflects every write the FUSE layer
/// passes through.
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use stratosync_core::{state::StateDb, types::SyncStatus, GlobSet};

use crate::sync::upload_queue::{UploadQueue, UploadTrigger};

// ── Watcher actor ─────────────────────────────────────────────────────────────

pub struct FsWatcher {
    _watcher:  RecommendedWatcher,   // keep alive
}

impl FsWatcher {
    /// Start watching `cache_dir` and forward relevant events to `upload_queue`.
    pub fn start(
        cache_dir:    PathBuf,
        mount_id:     u32,
        db:           Arc<StateDb>,
        upload_queue: Arc<UploadQueue>,
        ignore:       Arc<GlobSet>,
    ) -> Result<Self> {
        let (tx, mut rx) = mpsc::channel::<notify::Result<Event>>(512);

        // notify uses a std channel internally; bridge to tokio mpsc.
        // blocking_send only fails if the receiver is dropped (event handler
        // task died). We use eprintln because this callback runs on a system
        // thread outside the tracing subscriber context.
        let mut watcher = notify::recommended_watcher(move |res| {
            if let Err(e) = tx.blocking_send(res) {
                eprintln!("stratosync: watcher channel dead, events will be lost: {e}");
            }
        })?;

        watcher.watch(&cache_dir, RecursiveMode::Recursive)?;
        watcher.configure(Config::default().with_poll_interval(Duration::from_secs(2)))?;

        let cache_dir_owned = cache_dir;
        tokio::spawn(async move {
            while let Some(res) = rx.recv().await {
                match res {
                    Ok(event) => handle_event(event, mount_id, &cache_dir_owned, &db, &upload_queue, &ignore).await,
                    Err(e)    => warn!("inotify error: {e}"),
                }
            }
            warn!(mount_id, "watcher event loop exited — file change detection stopped");
        });

        Ok(Self { _watcher: watcher })
    }
}

// ── Event handler ─────────────────────────────────────────────────────────────

async fn handle_event(
    event:        Event,
    mount_id:     u32,
    cache_dir:    &std::path::Path,
    db:           &Arc<StateDb>,
    upload_queue: &Arc<UploadQueue>,
    ignore:       &GlobSet,
) {
    let is_write_event = matches!(
        event.kind,
        EventKind::Modify(_) | EventKind::Create(_)
    );
    if !is_write_event { return; }

    for path in &event.paths {
        // Skip partial files and temp files
        let fname = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if fname.ends_with(".tmp") || fname.starts_with('.') {
            continue;
        }

        // Derive remote_path from cache file path: strip cache_dir prefix.
        // This is more robust than looking up by cache_path column, because
        // the poller may replace inodes (changing the DB entry) while the
        // cache file stays at the same filesystem path.
        let remote_path = match path.strip_prefix(cache_dir) {
            Ok(rel) => match rel.to_str() {
                Some(s) => s.to_owned(),
                None    => continue,
            },
            Err(_) => continue,
        };

        // Selective sync: ignored paths must never enqueue uploads.
        if ignore.is_match(&remote_path) {
            continue;
        }

        // Look up the current inode by remote_path (stable across poller upserts)
        match db.get_by_remote_path(mount_id, &remote_path).await {
            Ok(Some(entry)) => {
                if entry.status == SyncStatus::Conflict {
                    continue; // conflict files must not be re-uploaded
                }
                // Only `dirty` rows carry a local edit. A `cached` row's
                // events are the daemon's own writes — chiefly hydration
                // renaming a finished download into place — and queueing
                // those re-uploaded every file that was merely read.
                // FUSE writes mark the row dirty (and enqueue) themselves;
                // this path remains for out-of-process writers such as
                // `stratosync versions restore`.
                if entry.status == SyncStatus::Dirty {
                    debug!(inode = entry.inode, path = ?path, event = ?event.kind, "fs event");
                    upload_queue.enqueue(UploadTrigger::Write { inode: entry.inode }).await;
                }
            }
            Ok(None) => {
                debug!(path = ?path, "untracked cache file modified");
            }
            Err(e) => warn!(path = ?path, "db lookup error: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::time::SystemTime;
    use notify::event::{ModifyKind, RenameMode, DataChange, CreateKind};
    use stratosync_core::{
        backend::mock::MockBackend,
        base_store::BaseStore,
        config::SyncConfig,
        state::NewFileEntry,
        types::{FileKind, Inode},
        Backend,
    };

    /// A queue with a long debounce: an enqueue shows up as `pending`
    /// in the snapshot and never actually runs during the test.
    async fn setup(status: SyncStatus) -> (Arc<StateDb>, Arc<UploadQueue>, u32, Inode, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(StateDb::in_memory().unwrap());
        db.migrate().await.unwrap();
        let mount_id = db.upsert_mount(
            "test", "mock:/", "/mnt/test",
            dir.path().to_str().unwrap(), 5 << 30, 60,
        ).await.unwrap();
        let root = db.insert_root(&NewFileEntry {
            mount_id, parent: 0, name: "/".into(), remote_path: "/".into(),
            kind: FileKind::Directory, size: 0, mtime: SystemTime::UNIX_EPOCH,
            etag: None, status: SyncStatus::Remote,
            cache_path: None, cache_size: None,
        }).await.unwrap();
        let inode = db.insert_file(&NewFileEntry {
            mount_id, parent: root, name: "photo.jpg".into(),
            remote_path: "Pictures/photo.jpg".into(),
            kind: FileKind::File, size: 3, mtime: SystemTime::UNIX_EPOCH,
            etag: Some("etag-1".into()), status,
            cache_path: Some(dir.path().join("Pictures/photo.jpg")),
            cache_size: Some(3),
        }).await.unwrap();
        let backend: Arc<dyn Backend> = Arc::new(MockBackend::default());
        let queue = Arc::new(UploadQueue::new(
            mount_id, Arc::clone(&db), backend,
            Arc::new(BaseStore::new(dir.path().join(".bases")).unwrap()),
            Arc::new(SyncConfig::default()),
            Duration::from_secs(600), Duration::from_secs(600), 1, None, 0,
        ));
        (db, queue, mount_id, inode, dir)
    }

    async fn fire(kind: EventKind, cache_dir: &Path, db: &Arc<StateDb>,
                  queue: &Arc<UploadQueue>, mount_id: u32) {
        let ev = Event::new(kind).add_path(cache_dir.join("Pictures/photo.jpg"));
        handle_event(ev, mount_id, cache_dir, db, queue, &GlobSet::empty()).await;
    }

    /// Live bug: hydration downloads to `.meta/partial/*.tmp`, renames
    /// into the cache, then marks the row `cached`. The watcher saw the
    /// rename as a Modify on a `cached` row and queued an upload — so
    /// every file *read* through the mount was re-uploaded (and, via the
    /// conflict resolver, downloaded a second time). A cached row has no
    /// recorded local edit; the event is the daemon's own write.
    #[tokio::test]
    async fn hydration_rename_into_cache_does_not_enqueue_upload() {
        let (db, queue, mid, _ino, dir) = setup(SyncStatus::Cached).await;
        fire(EventKind::Modify(ModifyKind::Name(RenameMode::To)), dir.path(), &db, &queue, mid).await;
        fire(EventKind::Create(CreateKind::File), dir.path(), &db, &queue, mid).await;
        fire(EventKind::Modify(ModifyKind::Data(DataChange::Any)), dir.path(), &db, &queue, mid).await;
        assert_eq!(queue.snapshot().await.pending, 0,
            "events on a cached (unmodified) file must not queue an upload");
    }

    /// The watcher is still the only upload trigger for writers outside
    /// the daemon process — `stratosync versions restore` writes the
    /// cache file and marks the row dirty, then relies on this path.
    #[tokio::test]
    async fn modify_on_dirty_row_still_enqueues_upload() {
        let (db, queue, mid, _ino, dir) = setup(SyncStatus::Dirty).await;
        fire(EventKind::Modify(ModifyKind::Data(DataChange::Any)), dir.path(), &db, &queue, mid).await;
        assert_eq!(queue.snapshot().await.pending, 1);
    }
}
