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

/// A pending upload waiting for its debounce window to expire.
struct PendingUpload {
    /// When the debounce expires and the upload should start.
    due_at: tokio::time::Instant,
}

async fn upload_loop(
    _tx_self:       mpsc::Sender<UploadTrigger>,
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
    // Debounce tracking: inode → when the upload should fire.
    // New triggers push the deadline forward (Write) or shorten it (Close/Fsync).
    let mut pending: HashMap<Inode, PendingUpload> = HashMap::new();
    let mut in_flight: JoinSet<(Inode, Result<(), SyncError>)> = JoinSet::new();

    loop {
        // Find the soonest deadline among pending items.
        let next_deadline = pending.values().map(|p| p.due_at).min();
        let at_capacity = in_flight.len() >= max_concurrent;

        tokio::select! {
            // Inbound trigger
            Some(trigger) = rx.recv() => {
                let (inode, window) = match trigger {
                    UploadTrigger::Write { inode } => (inode, debounce),
                    UploadTrigger::Close { inode } => (inode, close_debounce),
                    UploadTrigger::Fsync { inode } => (inode, Duration::ZERO),
                };

                let new_due = tokio::time::Instant::now() + window;

                match pending.get_mut(&inode) {
                    Some(entry) => {
                        // Already pending — only move the deadline EARLIER
                        // (Close/Fsync shorten the window, Write resets it).
                        if matches!(trigger, UploadTrigger::Write { .. }) {
                            // Write: reset debounce from now
                            entry.due_at = new_due;
                        } else {
                            // Close/Fsync: shorten to min(current, new)
                            entry.due_at = entry.due_at.min(new_due);
                        }
                    }
                    None => {
                        pending.insert(inode, PendingUpload { due_at: new_due });
                    }
                }
            }

            // A deadline expired — launch upload if under concurrency limit.
            // When at capacity, wait for a completion via join_next instead.
            _ = async {
                if at_capacity {
                    // Don't busy-loop on past deadlines while at capacity.
                    // The join_next arm will fire when a slot opens, then
                    // the next iteration will process the deadline.
                    std::future::pending::<()>().await;
                }
                match next_deadline {
                    Some(d) => tokio::time::sleep_until(d).await,
                    None    => std::future::pending().await,
                }
            } => {
                let now = tokio::time::Instant::now();
                let ready: Vec<Inode> = pending.iter()
                    .filter(|(_, p)| p.due_at <= now)
                    .map(|(&inode, _)| inode)
                    .collect();

                for inode in ready {
                    if in_flight.len() >= max_concurrent {
                        break;
                    }
                    pending.remove(&inode);

                    let db_c  = Arc::clone(&db);
                    let be_c  = Arc::clone(&backend);
                    let bs_c  = Arc::clone(&base_store);
                    let sc_c  = Arc::clone(&sync_config);

                    in_flight.spawn(async move {
                        let result = run_upload(inode, mount_id, &db_c, &be_c, &bs_c, &sc_c).await;
                        (inode, result)
                    });
                }
            }

            // Completed upload
            Some(result) = in_flight.join_next() => {
                match result {
                    Ok((inode, Ok(()))) => {
                        debug!(inode, "upload complete");
                    }
                    Ok((inode, Err(SyncError::Conflict { local, remote }))) => {
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
                    }
                    Ok((inode, Err(e))) if e.is_retryable() => {
                        warn!(inode, "upload transient error: {e} — will retry");
                        if let Err(db_err) = db.fail_queue_job_by_inode(inode, &e.to_string(), 30).await {
                            warn!(inode, "failed to record retry backoff: {db_err}");
                        }
                        // Re-add to pending with debounce delay for retry
                        pending.insert(inode, PendingUpload {
                            due_at: tokio::time::Instant::now() + debounce,
                        });
                    }
                    Ok((inode, Err(e))) => {
                        warn!(inode, "upload fatal: {e}");
                        if let Err(db_err) = db.set_status(inode, SyncStatus::Dirty).await {
                            warn!(inode, "failed to reset status to Dirty: {db_err}");
                        }
                        if let Ok(Some(entry)) = db.get_by_inode(inode).await {
                            super::notification::send(
                                "stratosync: upload failed",
                                &format!("Failed to upload '{}': {e}", entry.name),
                            );
                        }
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

    // Skip conflict files — they are stored under .stratosync-conflicts/
    // and must not be re-uploaded through the normal upload path.
    if entry.status == SyncStatus::Conflict {
        debug!(inode, "skipping upload of conflict file");
        return Ok(());
    }

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
