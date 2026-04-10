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
use tracing::{debug, error, info, warn};

use stratosync_core::{
    backend::Backend,
    base_store::BaseStore,
    config::SyncConfig,
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
        base_store:      Arc<BaseStore>,
        sync_config:     Arc<SyncConfig>,
        debounce:        Duration,
        close_debounce:  Duration,
        max_concurrent:  usize,
    ) -> Self {
        let (tx, rx) = mpsc::channel(512);
        let tx_inner = tx.clone();
        let db2 = Arc::clone(&db);
        tokio::spawn(async move {
            upload_loop(
                tx_inner, rx, mount_id, db, backend,
                base_store, sync_config,
                debounce, close_debounce, max_concurrent,
            ).await;
            // If we get here, the loop exited (channel closed or unexpected return).
            // Reset any stuck Uploading inodes so they're retried on restart.
            if let Err(e) = db2.reset_uploading().await {
                error!("failed to reset uploading inodes after upload loop exit: {e}");
            }
            error!("upload loop exited — file uploads will not work until daemon restart");
        });
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
    base_store:     Arc<BaseStore>,
    sync_config:    Arc<SyncConfig>,
    debounce:       Duration,
    close_debounce: Duration,
    max_concurrent: usize,
) {
    // inode → in-flight abort handle
    let mut pending: HashMap<Inode, tokio::sync::oneshot::Sender<()>> = HashMap::new();
    // (inode, result, was_aborted) — the bool distinguishes aborted debounce
    // timers from actual upload outcomes so we don't remove the wrong pending entry.
    let mut in_flight: JoinSet<(Inode, Result<(), SyncError>, bool)> = JoinSet::new();

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
                let had_pending = pending.remove(&inode).map(|abort| {
                    let _ = abort.send(());
                }).is_some();

                // Respect concurrency limit — but if we just aborted an
                // existing task for this inode, allow the replacement through
                // (it's recycling the same slot, not adding a new one).
                if !had_pending && in_flight.len() >= max_concurrent {
                    // At capacity with no existing task to replace. Re-queue
                    // via the async send (not try_send) to apply backpressure
                    // rather than silently dropping the trigger.
                    let tx = tx_self.clone();
                    tokio::spawn(async move {
                        let _ = tx.send(UploadTrigger::Write { inode }).await;
                    });
                    continue;
                }

                let (abort_tx, abort_rx) = tokio::sync::oneshot::channel();
                pending.insert(inode, abort_tx);

                let db_c  = Arc::clone(&db);
                let be_c  = Arc::clone(&backend);
                let bs_c  = Arc::clone(&base_store);
                let sc_c  = Arc::clone(&sync_config);

                in_flight.spawn(async move {
                    // Wait for window, unless aborted (new write came in,
                    // or the pending entry was removed by a prior task's
                    // completion handler — which drops the sender).
                    tokio::select! {
                        _ = sleep(window) => {}
                        _ = abort_rx      => {
                            // Aborted — signal caller to NOT remove from pending.
                            return (inode, Err(SyncError::Transient("debounce aborted".into())), true);
                        }
                    }

                    let result = run_upload(inode, mount_id, &db_c, &be_c, &bs_c, &sc_c).await;
                    (inode, result, false)
                });
            }

            // Completed upload
            Some(result) = in_flight.join_next() => {
                match result {
                    // Debounce was aborted (new write reset the timer, or the
                    // pending entry's sender was dropped). Do NOT remove from
                    // pending — the replacement task owns the current entry.
                    Ok((_, _, true)) => {}

                    Ok((inode, Ok(()), _)) => {
                        debug!(inode, "upload complete");
                        pending.remove(&inode);
                    }
                    Ok((inode, Err(SyncError::Conflict { local, remote }), _)) => {
                        warn!(inode, ?local, ?remote, "upload conflict — invoking resolver");
                        if let Ok(Some(entry)) = db.get_by_inode(inode).await {
                            let has_git = super::conflict::git_available();
                            if let Err(e) = super::conflict::resolve(
                                &entry, &db, &backend,
                                &base_store, &sync_config, has_git,
                            ).await {
                                warn!(inode, "conflict resolution failed: {e}");
                            }
                        }
                        pending.remove(&inode);
                    }
                    Ok((inode, Err(e), _)) if e.is_retryable() => {
                        warn!(inode, "upload transient error: {e} — will retry");
                        if let Err(db_err) = db.fail_queue_job_by_inode(inode, &e.to_string(), 30).await {
                            warn!(inode, "failed to record retry backoff: {db_err}");
                        }
                        pending.remove(&inode);
                        if let Err(q_err) = tx_self.try_send(UploadTrigger::Write { inode }) {
                            warn!(inode, "retry re-queue failed (channel full/closed): {q_err}");
                        }
                    }
                    Ok((inode, Err(e), _)) => {
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
    inode:       Inode,
    mount_id:    u32,
    db:          &Arc<StateDb>,
    backend:     &Arc<dyn Backend>,
    base_store:  &Arc<BaseStore>,
    sync_config: &Arc<SyncConfig>,
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

    // Reject symlinks — prevents uploading arbitrary files via cache dir manipulation
    let file_meta = tokio::fs::symlink_metadata(&cache_path).await
        .map_err(SyncError::Io)?;
    if file_meta.file_type().is_symlink() {
        return Err(SyncError::Fatal(format!("cache file is a symlink: {}", cache_path.display())));
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

    // Snapshot base version for 3-way merge (best-effort)
    let max_size = sync_config.base_max_file_size_bytes().unwrap_or(10 * 1024 * 1024);
    if BaseStore::is_text_mergeable(&cache_path, meta.size, max_size, &sync_config.text_extensions) {
        let bs = Arc::clone(base_store);
        let cp = cache_path.clone();
        let db2 = Arc::clone(db);
        let mid = mount_id;
        tokio::task::spawn_blocking(move || {
            match bs.store_base(&cp) {
                Ok(hash) => {
                    let _ = tokio::runtime::Handle::current().block_on(
                        db2.set_base_hash(inode, mid, &hash, 0)
                    );
                    debug!(inode, %hash, "base version captured on upload");
                }
                Err(e) => {
                    warn!(inode, "failed to capture base version after upload: {e}");
                }
            }
        });
    }

    info!(inode, path = %entry.remote_path, "upload ok");
    Ok(())
}
