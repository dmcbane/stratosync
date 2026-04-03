#![allow(dead_code, unused_imports)]
/// UploadQueue — coalesces writes and schedules debounced uploads.
///
/// Design: each dirty inode has one pending `UploadJob`.  A tokio
/// `JoinSet` runs per-inode timer tasks that fire after the debounce
/// window expires.  `fsync()` or `close()` can shorten the window.
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinSet;
use tokio::time::sleep;
use tracing::{debug, info, warn};

use stratosync_core::{
    backend::Backend,
    state::{StateDb, SyncQueueJob},
    types::{Inode, SyncError, SyncStatus},
};

// ── Public API ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum UploadTrigger {
    /// Normal write — reset debounce window.
    Write { inode: Inode },
    /// File closed — use short debounce window.
    Close { inode: Inode },
    /// fsync — upload immediately.
    Fsync { inode: Inode },
}

pub struct UploadQueue {
    tx: mpsc::Sender<UploadTrigger>,
}

impl UploadQueue {
    pub fn new(
        mount_id:        u32,
        db:              Arc<StateDb>,
        backend:         Arc<dyn Backend>,
        debounce:        Duration,
        close_debounce:  Duration,
        max_concurrent:  usize,
    ) -> Self {
        let (tx, rx) = mpsc::channel(512);
        tokio::spawn(upload_loop(
            tx.clone(), rx, mount_id, db, backend,
            debounce, close_debounce, max_concurrent,
        ));
        Self { tx }
    }

    pub async fn enqueue(&self, trigger: UploadTrigger) {
        if let Err(e) = self.tx.send(trigger).await {
            // Channel closed = upload_loop task died. Writes will not sync
            // until the daemon is restarted.
            warn!("upload queue send failed (loop dead?): {e}");
        }
    }
}

// ── Internal loop ─────────────────────────────────────────────────────────────

struct PendingUpload {
    inode:      Inode,
    due_at:     Instant,
    abort_tx:   tokio::sync::oneshot::Sender<()>,
}

async fn upload_loop(
    tx_self:        mpsc::Sender<UploadTrigger>,
    mut rx:         mpsc::Receiver<UploadTrigger>,
    mount_id:       u32,
    db:             Arc<StateDb>,
    backend:        Arc<dyn Backend>,
    debounce:       Duration,
    close_debounce: Duration,
    max_concurrent: usize,
) {
    // inode → in-flight abort handle
    let mut pending: HashMap<Inode, tokio::sync::oneshot::Sender<()>> = HashMap::new();
    let mut in_flight: JoinSet<(Inode, Result<(), SyncError>)> = JoinSet::new();

    loop {
        tokio::select! {
            // Inbound trigger
            Some(trigger) = rx.recv() => {
                let (inode, window) = match trigger {
                    UploadTrigger::Write { inode } => (inode, debounce),
                    UploadTrigger::Close { inode } => (inode, close_debounce),
                    UploadTrigger::Fsync { inode } => (inode, Duration::ZERO),
                };

                // Cancel any existing timer for this inode.
                // Receiver dropped = timer already fired; that's fine.
                if let Some(abort) = pending.remove(&inode) {
                    let _ = abort.send(());
                }

                // Respect concurrency limit
                if in_flight.len() >= max_concurrent {
                    // Re-queue with full debounce to avoid starvation.
                    if let Err(e) = tx_self.try_send(UploadTrigger::Write { inode }) {
                        warn!(inode, "re-queue failed (channel full/closed): {e}");
                    }
                    continue;
                }

                let (abort_tx, abort_rx) = tokio::sync::oneshot::channel();
                pending.insert(inode, abort_tx);

                let db_c  = Arc::clone(&db);
                let be_c  = Arc::clone(&backend);

                in_flight.spawn(async move {
                    // Wait for window, unless aborted
                    tokio::select! {
                        _ = sleep(window) => {}
                        _ = abort_rx      => {
                            // Aborted (new write came in); caller re-enqueues
                            return (inode, Ok(()));
                        }
                    }

                    let result = run_upload(inode, mount_id, &db_c, &be_c).await;
                    (inode, result)
                });
            }

            // Completed upload
            Some(result) = in_flight.join_next() => {
                match result {
                    Ok((inode, Ok(()))) => {
                        debug!(inode, "upload complete");
                        pending.remove(&inode);
                    }
                    Ok((inode, Err(SyncError::Conflict { local, remote }))) => {
                        warn!(inode, ?local, ?remote, "upload conflict — invoking resolver");
                        // ConflictResolver is called inline; see below
                        pending.remove(&inode);
                    }
                    Ok((inode, Err(e))) if e.is_retryable() => {
                        warn!(inode, "upload transient error: {e} — will retry");
                        if let Err(db_err) = db.fail_queue_job_by_inode(inode, &e.to_string(), 30).await {
                            warn!(inode, "failed to record retry backoff: {db_err}");
                        }
                        pending.remove(&inode);
                        if let Err(q_err) = tx_self.try_send(UploadTrigger::Write { inode }) {
                            warn!(inode, "retry re-queue failed (channel full/closed): {q_err}");
                        }
                    }
                    Ok((inode, Err(e))) => {
                        warn!(inode, "upload fatal: {e}");
                        // Reset to Dirty so the file is retried on next daemon start.
                        // If this fails, the inode stays in Uploading; startup recovery
                        // (get_pending_uploads) handles that case.
                        if let Err(db_err) = db.set_status(inode, SyncStatus::Dirty).await {
                            warn!(inode, "failed to reset status to Dirty: {db_err}");
                        }
                        pending.remove(&inode);
                    }
                    Err(join_err) => {
                        warn!("upload task panicked: {join_err}");
                    }
                }
            }

            else => break,
        }
    }
}

async fn run_upload(
    inode:    Inode,
    _mount_id: u32,
    db:       &Arc<StateDb>,
    backend:  &Arc<dyn Backend>,
) -> Result<(), SyncError> {
    // Load the job spec from DB
    let entry = db.get_by_inode(inode).await
        .map_err(|e| SyncError::Fatal(e.to_string()))?
        .ok_or_else(|| SyncError::NotFound(format!("inode {inode}")))?;

    let cache_path = entry.cache_path
        .ok_or_else(|| SyncError::Fatal(format!("inode {inode}: dirty but no cache_path")))?;

    if !cache_path.exists() {
        return Err(SyncError::Fatal(format!("cache file missing: {}", cache_path.display())));
    }

    // Mark as UPLOADING before we start
    db.set_status(inode, SyncStatus::Uploading).await
        .map_err(|e| SyncError::Fatal(e.to_string()))?;

    info!(inode, path = %entry.remote_path, "uploading");

    let meta = backend.upload(
        &cache_path,
        &entry.remote_path,
        entry.etag.as_deref(),
    ).await?;

    // Success — update etag and mark CACHED
    db.set_cached(
        inode,
        &cache_path,
        meta.size,
        meta.etag.as_deref(),
        meta.mtime,
        meta.size,
    ).await.map_err(|e| SyncError::Fatal(e.to_string()))?;

    info!(inode, path = %entry.remote_path, "upload ok");
    Ok(())
}
