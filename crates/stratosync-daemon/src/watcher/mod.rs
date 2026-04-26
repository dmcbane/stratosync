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
                if matches!(entry.status, SyncStatus::Cached | SyncStatus::Dirty) {
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
