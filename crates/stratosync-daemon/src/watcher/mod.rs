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

use stratosync_core::{state::StateDb, types::SyncStatus};

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
    ) -> Result<Self> {
        let (tx, mut rx) = mpsc::channel::<notify::Result<Event>>(512);

        // notify uses a std channel internally; bridge to tokio mpsc
        let mut watcher = notify::recommended_watcher(move |res| {
            let _ = tx.blocking_send(res);
        })?;

        watcher.watch(&cache_dir, RecursiveMode::Recursive)?;
        watcher.configure(Config::default().with_poll_interval(Duration::from_secs(2)))?;

        tokio::spawn(async move {
            while let Some(res) = rx.recv().await {
                match res {
                    Ok(event) => handle_event(event, mount_id, &db, &upload_queue).await,
                    Err(e)    => warn!("inotify error: {e}"),
                }
            }
        });

        Ok(Self { _watcher: watcher })
    }
}

// ── Event handler ─────────────────────────────────────────────────────────────

async fn handle_event(
    event:        Event,
    mount_id:     u32,
    db:           &Arc<StateDb>,
    upload_queue: &Arc<UploadQueue>,
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

        // Look up the inode by cache_path
        match db.get_by_cache_path(mount_id, path).await {
            Ok(Some(entry)) => {
                if matches!(entry.status, SyncStatus::Cached | SyncStatus::Dirty) {
                    debug!(inode = entry.inode, path = ?path, event = ?event.kind, "fs event");
                    upload_queue.enqueue(UploadTrigger::Write { inode: entry.inode }).await;
                }
            }
            Ok(None) => {
                // File in cache dir not tracked yet — could be a new file
                // created externally; ignore for now (Phase 2: handle auto-import)
                debug!(path = ?path, "untracked cache file modified");
            }
            Err(e) => warn!(path = ?path, "db lookup error: {e}"),
        }
    }
}
