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
use dashmap::DashMap;
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinSet;
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

use chrono::{Local, Timelike};

use stratosync_core::{
    backend::Backend,
    base_store::BaseStore,
    config::{SyncConfig, UploadWindow},
    ipc::{ActiveUpload, QueueStatus},
    state::{StateDb, SyncQueueJob, VersionSource},
    types::{Inode, SyncError, SyncStatus},
};

// ── Public API ────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum UploadTrigger {
    /// Normal write — reset debounce window.
    Write { inode: Inode },
    /// File closed — use short debounce window.
    Close { inode: Inode },
    /// fsync — upload immediately.
    Fsync { inode: Inode },
    /// Snapshot current queue state for the dashboard. Never persisted.
    Snapshot { reply: tokio::sync::oneshot::Sender<QueueStatus> },
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
        upload_window:   Option<UploadWindow>,
        version_retention: u32,
    ) -> Self {
        let (tx, rx) = mpsc::channel(512);
        let tx_inner = tx.clone();
        let db2 = Arc::clone(&db);
        tokio::spawn(async move {
            upload_loop(
                tx_inner, rx, mount_id, db, backend,
                base_store, sync_config,
                debounce, close_debounce, max_concurrent,
                upload_window, version_retention,
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

    /// Request a live snapshot of queue state (pending count + in-flight
    /// uploads with start times). Returns a default/empty snapshot if the
    /// upload loop is dead.
    pub async fn snapshot(&self) -> QueueStatus {
        let (reply, rx) = tokio::sync::oneshot::channel();
        if self.tx.send(UploadTrigger::Snapshot { reply }).await.is_err() {
            return QueueStatus::default();
        }
        rx.await.unwrap_or_default()
    }
}

// ── Internal loop ─────────────────────────────────────────────────────────────

/// A pending upload waiting for its debounce window to expire.
struct PendingUpload {
    /// When the debounce expires and the upload should start.
    due_at: tokio::time::Instant,
    /// True if any trigger for this inode was an explicit `fsync` —
    /// such uploads bypass the bandwidth schedule. Once set, stays
    /// set until the upload runs (a subsequent Write does not
    /// downgrade an fsync'd entry).
    immediate: bool,
}

/// Compute the soonest tokio::time::Instant the upload may fire for a
/// pending entry, given the bandwidth window. Immediate (fsync'd) entries
/// always fire at their natural `due_at`; non-immediate entries are pushed
/// to whichever is later: their `due_at`, or when the window opens.
fn effective_due_at(
    p:              &PendingUpload,
    window:         Option<UploadWindow>,
    now_tokio:      tokio::time::Instant,
    secs_until_open: u64,
) -> tokio::time::Instant {
    if p.immediate || window.is_none() || secs_until_open == 0 {
        return p.due_at;
    }
    let opens_at = now_tokio + Duration::from_secs(secs_until_open);
    p.due_at.max(opens_at)
}

/// Seconds until the upload window opens, or 0 if it's open right now
/// (or no window is configured).
fn secs_until_window_opens(window: Option<UploadWindow>) -> u64 {
    let Some(w) = window else { return 0 };
    let now = Local::now();
    let now_min = now.hour() * 60 + now.minute();
    let now_sec = now.second();
    w.seconds_until_open(now_min, now_sec)
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
    upload_window:  Option<UploadWindow>,
    version_retention: u32,
) {
    // Debounce tracking: inode → when the upload should fire.
    // New triggers push the deadline forward (Write) or shorten it (Close/Fsync).
    let mut pending: HashMap<Inode, PendingUpload> = HashMap::new();
    let mut in_flight: JoinSet<(Inode, Result<(), SyncError>)> = JoinSet::new();
    // Parallel to `in_flight`: tracks when the *current attempt* started
    // (resets each retry) and supports the dashboard's "elapsed" column.
    let mut in_flight_started: HashMap<Inode, Instant> = HashMap::new();
    // When the inode *first* entered the in-flight state. Preserved
    // across retryable errors so the dashboard can distinguish "fresh
    // 12s upload" from "20-minute retry loop." Cleared on success,
    // fatal, or conflict (i.e. when the inode actually leaves the
    // queue).
    let mut in_flight_first_started: HashMap<Inode, Instant> = HashMap::new();
    // Attempt counter per inode. 1 on the initial spawn, +1 on each
    // retryable error → re-spawn. Same lifecycle as `in_flight_first_started`.
    let mut attempts: HashMap<Inode, u32> = HashMap::new();
    // Live byte-progress for the current attempt, written by the
    // spawned upload task as rclone emits `--stats=1s` lines. Shared
    // (Arc<DashMap>) so the snapshot path can read without contending
    // with active uploads. Resets on every spawn (a new attempt is a
    // new transfer that re-uploads from byte zero).
    let in_flight_progress: Arc<DashMap<Inode, u64>> = Arc::new(DashMap::new());

    loop {
        // Bandwidth schedule: how long until the window reopens (0 if open
        // right now or no schedule). Computed once per loop iteration so the
        // deadline calc and dispatch check use a consistent snapshot.
        let secs_until_open = secs_until_window_opens(upload_window);
        let now_tokio = tokio::time::Instant::now();

        // Find the soonest deadline among pending items, factoring in the
        // bandwidth window — non-immediate jobs whose natural `due_at` lands
        // outside the window get pushed to when the window opens, so the
        // sleep_until below doesn't busy-spin re-checking past deadlines.
        let next_deadline = pending.values()
            .map(|p| effective_due_at(p, upload_window, now_tokio, secs_until_open))
            .min();
        let at_capacity = in_flight.len() >= max_concurrent;

        tokio::select! {
            // Inbound trigger
            Some(trigger) = rx.recv() => {
                // Handle the dashboard snapshot op separately — it doesn't
                // touch the pending map.
                if let UploadTrigger::Snapshot { reply } = trigger {
                    let snapshot = build_queue_snapshot(
                        &pending,
                        &in_flight_started,
                        &in_flight_first_started,
                        &attempts,
                        &in_flight_progress,
                        &db,
                    ).await;
                    let _ = reply.send(snapshot);
                    continue;
                }

                let (inode, window, is_fsync) = match trigger {
                    UploadTrigger::Write { inode } => (inode, debounce,        false),
                    UploadTrigger::Close { inode } => (inode, close_debounce,  false),
                    UploadTrigger::Fsync { inode } => (inode, Duration::ZERO,  true),
                    UploadTrigger::Snapshot { .. } => unreachable!("handled above"),
                };

                let new_due = tokio::time::Instant::now() + window;

                match pending.get_mut(&inode) {
                    Some(entry) => {
                        // Already pending — only move the deadline EARLIER
                        // (Close/Fsync shorten the window, Write resets it).
                        if window == debounce {
                            // Write: reset debounce from now
                            entry.due_at = new_due;
                        } else {
                            // Close/Fsync: shorten to min(current, new)
                            entry.due_at = entry.due_at.min(new_due);
                        }
                        // Once fsync'd, stay fsync'd — never downgrade. A
                        // subsequent Write that arrives mid-flight just adds
                        // bytes; user already said "I care about durability."
                        if is_fsync {
                            entry.immediate = true;
                        }
                    }
                    None => {
                        pending.insert(inode, PendingUpload {
                            due_at:    new_due,
                            immediate: is_fsync,
                        });
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
                // Re-check the window — it may have just opened (sleep_until
                // landed exactly at the boundary) or, conversely, ticked past
                // a non-wrapping window's end while we slept.
                let window_open_now = secs_until_window_opens(upload_window) == 0;
                let ready: Vec<Inode> = pending.iter()
                    .filter(|(_, p)| {
                        // Job is ready if its debounce has expired AND
                        // (it's an fsync OR the bandwidth window is open).
                        p.due_at <= now && (p.immediate || window_open_now)
                    })
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

                    // Set up the per-attempt progress channel. The
                    // upload task pushes raw bytes-uploaded counts; a
                    // small forwarder task copies those into the shared
                    // DashMap the dashboard reads. Bounded channel so a
                    // hung dashboard can never back up rclone's stderr
                    // pipe — we drop progress updates instead.
                    let (prog_tx, mut prog_rx) = mpsc::channel::<u64>(8);
                    let progress_map = Arc::clone(&in_flight_progress);
                    progress_map.insert(inode, 0);
                    tokio::spawn(async move {
                        while let Some(bytes) = prog_rx.recv().await {
                            progress_map.insert(inode, bytes);
                        }
                    });

                    in_flight.spawn(async move {
                        let result = run_upload(
                            inode, mount_id, &db_c, &be_c, &bs_c, &sc_c,
                            version_retention, prog_tx,
                        ).await;
                        (inode, result)
                    });
                    let now = Instant::now();
                    in_flight_started.insert(inode, now);
                    // first_started is preserved across retries: only
                    // record on the first entry into the queue.
                    in_flight_first_started.entry(inode).or_insert(now);
                    *attempts.entry(inode).or_insert(0) += 1;
                }
            }

            // Completed upload
            Some(result) = in_flight.join_next() => {
                // Helper: the inode has truly left the queue (success,
                // conflict, or fatal — anything that doesn't reschedule
                // a retry). Drops the current-attempt timer, the
                // first-started timer, the attempt counter, and the
                // progress entry. The forwarder task on the prog
                // channel exits naturally when its sender is dropped
                // by `run_upload`'s task ending.
                let clear_all = |inode: Inode,
                                     in_flight_started: &mut HashMap<Inode, Instant>,
                                     in_flight_first_started: &mut HashMap<Inode, Instant>,
                                     attempts: &mut HashMap<Inode, u32>,
                                     in_flight_progress: &Arc<DashMap<Inode, u64>>| {
                    in_flight_started.remove(&inode);
                    in_flight_first_started.remove(&inode);
                    attempts.remove(&inode);
                    in_flight_progress.remove(&inode);
                };

                match result {
                    Ok((inode, Ok(()))) => {
                        clear_all(inode, &mut in_flight_started,
                                  &mut in_flight_first_started,
                                  &mut attempts, &in_flight_progress);
                        debug!(inode, "upload complete");
                    }
                    Ok((inode, Err(SyncError::Conflict { local, remote }))) => {
                        clear_all(inode, &mut in_flight_started,
                                  &mut in_flight_first_started,
                                  &mut attempts, &in_flight_progress);
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
                        // Retryable: drop the *current attempt* timer
                        // and progress, but preserve `first_started`
                        // and `attempts` so the dashboard can show
                        // "attempt 3, started 4m ago, retrying".
                        in_flight_started.remove(&inode);
                        in_flight_progress.remove(&inode);
                        let n = attempts.get(&inode).copied().unwrap_or(1);
                        warn!(inode, attempt = n,
                              "upload transient error: {e} — will retry");
                        if let Err(db_err) = db.fail_queue_job_by_inode(inode, &e.to_string(), 30).await {
                            warn!(inode, "failed to record retry backoff: {db_err}");
                        }
                        // Re-add to pending with debounce delay for retry.
                        // Retries are not "immediate" — they respect the
                        // bandwidth window like a normal write.
                        pending.insert(inode, PendingUpload {
                            due_at:    tokio::time::Instant::now() + debounce,
                            immediate: false,
                        });
                    }
                    Ok((inode, Err(e))) => {
                        clear_all(inode, &mut in_flight_started,
                                  &mut in_flight_first_started,
                                  &mut attempts, &in_flight_progress);
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
                        // JoinError doesn't carry the inode; we can't remove
                        // the metadata entry. It will be cleaned up the next
                        // time this inode gets picked up. This is rare.
                        warn!("upload task panicked: {join_err}");
                    }
                }
            }

            else => break,
        }
    }
}

/// Build a QueueStatus snapshot for the dashboard IPC. Looks up each
/// in-flight inode in the DB to fetch its path and size, then folds in
/// the per-attempt timers, the first-attempt timer, attempt count, and
/// live byte progress.
async fn build_queue_snapshot(
    pending: &HashMap<Inode, PendingUpload>,
    in_flight_started: &HashMap<Inode, Instant>,
    in_flight_first_started: &HashMap<Inode, Instant>,
    attempts: &HashMap<Inode, u32>,
    in_flight_progress: &Arc<DashMap<Inode, u64>>,
    db: &Arc<StateDb>,
) -> QueueStatus {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let mut in_flight: Vec<ActiveUpload> = Vec::with_capacity(in_flight_started.len());
    for (&inode, &started) in in_flight_started {
        let (path, size) = match db.get_by_inode(inode).await {
            Ok(Some(e)) => (e.remote_path, e.size),
            _ => (String::from("(unknown)"), 0),
        };
        let elapsed = started.elapsed().as_secs() as i64;
        // first_started is preserved across retries; if for some reason
        // it's missing (race with completion), fall back to the current
        // attempt's start.
        let first_elapsed = in_flight_first_started.get(&inode)
            .map(|t| t.elapsed().as_secs() as i64)
            .unwrap_or(elapsed);
        let attempt = attempts.get(&inode).copied().unwrap_or(1);
        // bytes_uploaded = None when the backend hasn't reported any
        // progress yet — distinguishes "0 bytes uploaded" from "we
        // don't know." rclone takes 1s for its first stats tick, so
        // sub-second uploads complete with no progress entry.
        let bytes_uploaded = in_flight_progress.get(&inode)
            .map(|v| *v.value());

        in_flight.push(ActiveUpload {
            inode,
            path,
            size_bytes: size,
            started_at_unix: now - elapsed,
            first_started_unix: now - first_elapsed,
            attempt,
            bytes_uploaded,
        });
    }
    QueueStatus {
        pending: pending.len() as u64,
        in_flight,
    }
}

async fn run_upload(
    inode:       Inode,
    mount_id:    u32,
    db:          &Arc<StateDb>,
    backend:     &Arc<dyn Backend>,
    base_store:  &Arc<BaseStore>,
    sync_config: &Arc<SyncConfig>,
    version_retention: u32,
    progress:    mpsc::Sender<u64>,
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

    // Defense in depth: directories don't have a cache_path so they
    // can't be uploaded, even if a stale row in the DB has them
    // marked dirty. `get_pending_uploads` filters these out at the
    // source, but a write/rename trigger could still hand us an
    // invalid inode. Quietly reset to Cached and move on instead of
    // returning a Fatal that spams desktop notifications.
    if entry.kind != stratosync_core::types::FileKind::File {
        debug!(inode, kind = ?entry.kind, "non-file entry queued for upload — resetting to cached");
        if let Err(e) = db.set_status(inode, SyncStatus::Cached).await {
            warn!(inode, "failed to clear non-file dirty status: {e}");
        }
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

    let meta = backend.upload_with_progress(
        &cache_path,
        &entry.remote_path,
        entry.etag.as_deref(),
        progress,
    ).await?;

    // Success — update etag and mark CACHED. Use `set_uploaded` (not
    // `set_cached`) so a rename that landed mid-upload keeps its new
    // cache_path. See the rename-during-upload regression test.
    db.set_uploaded(
        inode,
        meta.size,
        meta.etag.as_deref(),
        meta.mtime,
        meta.size,
    ).await.map_err(|e| SyncError::Fatal(e.to_string()))?;

    // Versioning: record the just-uploaded content as a historical
    // snapshot. Best-effort — failure here doesn't roll back the upload.
    let max_size = sync_config.base_max_file_size_bytes().unwrap_or(10 * 1024 * 1024);
    if let Err(e) = super::versioning::capture(
        db, base_store, inode, mount_id, &cache_path,
        meta.size, meta.etag.as_deref(),
        VersionSource::AfterUpload,
        max_size, version_retention,
    ).await {
        warn!(inode, "post-upload version snapshot failed: {e}");
    }

    // Snapshot base version for 3-way merge (best-effort)
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

#[cfg(test)]
mod tests {
    //! Unit tests for the bandwidth-scheduling helpers.
    //!
    //! End-to-end dispatcher tests would require injecting a clock for
    //! `chrono::Local::now()`, which the codebase doesn't have abstracted
    //! today. The dispatcher's behavior follows mechanically from
    //! `effective_due_at` + the trigger handling, so we test those at the
    //! unit level and rely on the type system for the wiring.
    use super::*;

    fn window(start: u32, end: u32) -> UploadWindow {
        UploadWindow { start_min: start, end_min: end }
    }

    fn pending(due_in: Duration, immediate: bool) -> PendingUpload {
        PendingUpload {
            due_at: tokio::time::Instant::now() + due_in,
            immediate,
        }
    }

    #[test]
    fn effective_due_at_immediate_bypasses_window() {
        let now = tokio::time::Instant::now();
        let p = pending(Duration::from_secs(1), true);
        // 1 hour until window opens, but immediate=true should ignore it.
        let got = effective_due_at(&p, Some(window(22 * 60, 6 * 60)), now, 3600);
        assert_eq!(got, p.due_at, "fsync'd entry must fire at its natural due_at");
    }

    #[test]
    fn effective_due_at_no_window_uses_natural_deadline() {
        let now = tokio::time::Instant::now();
        let p = pending(Duration::from_secs(5), false);
        let got = effective_due_at(&p, None, now, 0);
        assert_eq!(got, p.due_at);
    }

    #[test]
    fn effective_due_at_window_open_uses_natural_deadline() {
        let now = tokio::time::Instant::now();
        let p = pending(Duration::from_secs(5), false);
        // Window present and open (secs_until_open == 0).
        let got = effective_due_at(&p, Some(window(0, 24 * 60 - 1)), now, 0);
        assert_eq!(got, p.due_at);
    }

    #[test]
    fn effective_due_at_closed_window_pushes_deadline_to_open_time() {
        let now = tokio::time::Instant::now();
        let p = pending(Duration::from_secs(5), false);
        // Job is naturally due in 5s, but the window opens in 600s.
        // Effective deadline must be the LATER of the two — i.e. window-open.
        let got = effective_due_at(&p, Some(window(22 * 60, 6 * 60)), now, 600);
        let expected = now + Duration::from_secs(600);
        assert_eq!(got, expected, "non-immediate job must wait for window open");
    }

    #[test]
    fn effective_due_at_late_natural_deadline_unchanged_inside_closed_window() {
        let now = tokio::time::Instant::now();
        // Natural debounce expires AFTER the window opens — the job should
        // fire at its natural deadline (the window will already be open).
        let p = pending(Duration::from_secs(1200), false);
        let got = effective_due_at(&p, Some(window(22 * 60, 6 * 60)), now, 600);
        assert_eq!(got, p.due_at, "max() picks the later of natural-due and window-open");
    }

    #[tokio::test]
    async fn secs_until_window_opens_returns_zero_for_no_window() {
        assert_eq!(secs_until_window_opens(None), 0);
    }

    #[tokio::test]
    async fn secs_until_window_opens_returns_zero_for_always_open_window() {
        // 12:00–12:00 is the degenerate "always open" form.
        let w = Some(window(12 * 60, 12 * 60));
        assert_eq!(secs_until_window_opens(w), 0);
    }

    // ── build_queue_snapshot semantics ────────────────────────────────

    use stratosync_core::state::{NewFileEntry, StateDb};
    use stratosync_core::types::{FileKind, SyncStatus};
    use std::path::PathBuf;
    use std::time::SystemTime;

    async fn make_db_with_inode(remote_path: &str, size: u64) -> (Arc<StateDb>, u32, Inode) {
        let db = StateDb::in_memory().unwrap();
        db.migrate().await.unwrap();
        let mount_id = db.upsert_mount(
            "test", "gdrive:/", "/mnt/test", "/tmp/cache",
            5 * 1024 * 1024 * 1024, 60,
        ).await.unwrap();
        let root = db.insert_root(&NewFileEntry {
            mount_id, parent: 0, name: "/".into(), remote_path: "/".into(),
            kind: FileKind::Directory, size: 0, mtime: SystemTime::UNIX_EPOCH,
            etag: None, status: SyncStatus::Remote,
            cache_path: None, cache_size: None,
        }).await.unwrap();
        let inode = db.insert_file(&NewFileEntry {
            mount_id, parent: root,
            name: remote_path.trim_start_matches('/').into(),
            remote_path: remote_path.into(),
            kind: FileKind::File, size, mtime: SystemTime::UNIX_EPOCH,
            etag: None, status: SyncStatus::Uploading,
            cache_path: Some(PathBuf::from("/tmp/cache/x")),
            cache_size: Some(size),
        }).await.unwrap();
        (Arc::new(db), mount_id, inode)
    }

    #[tokio::test]
    async fn snapshot_carries_attempt_and_first_started_for_retry() {
        let (db, _mid, inode) = make_db_with_inode("/big.iso", 50_000_000).await;

        // Simulate: first attempt began ~120s ago, current attempt
        // (after retry) began 10s ago, this is attempt #3, and the
        // backend has reported 5 MiB transferred for the current go.
        let now = Instant::now();
        let mut started = HashMap::new();
        started.insert(inode, now - Duration::from_secs(10));
        let mut first = HashMap::new();
        first.insert(inode, now - Duration::from_secs(120));
        let mut attempts = HashMap::new();
        attempts.insert(inode, 3);
        let progress: Arc<DashMap<Inode, u64>> = Arc::new(DashMap::new());
        progress.insert(inode, 5 * 1024 * 1024);

        let snap = build_queue_snapshot(
            &HashMap::new(), &started, &first, &attempts, &progress, &db,
        ).await;

        assert_eq!(snap.in_flight.len(), 1);
        let up = &snap.in_flight[0];
        assert_eq!(up.inode, inode);
        assert_eq!(up.attempt, 3);
        assert_eq!(up.size_bytes, 50_000_000);
        assert_eq!(up.bytes_uploaded, Some(5 * 1024 * 1024));
        // The current-attempt timer says ~10s elapsed, the first-attempt
        // timer says ~120s. Allow ±2s slop for test scheduling.
        let cur_elapsed   = (up.size_bytes as i64).max(0); // suppress unused on size in case
        let _ = cur_elapsed;
        let now_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
        assert!((now_unix - up.started_at_unix - 10).abs() <= 2,
                "current-attempt elapsed should be ~10s, got {}",
                now_unix - up.started_at_unix);
        assert!((now_unix - up.first_started_unix - 120).abs() <= 2,
                "first-attempt elapsed should be ~120s, got {}",
                now_unix - up.first_started_unix);
    }

    #[tokio::test]
    async fn snapshot_defaults_attempt_to_one_when_map_is_empty() {
        let (db, _mid, inode) = make_db_with_inode("/x.txt", 100).await;
        let mut started = HashMap::new();
        started.insert(inode, Instant::now());
        let progress: Arc<DashMap<Inode, u64>> = Arc::new(DashMap::new());

        let snap = build_queue_snapshot(
            &HashMap::new(), &started,
            &HashMap::new(),    // first_started missing — fall back path
            &HashMap::new(),    // attempts missing — defaults to 1
            &progress, &db,
        ).await;
        let up = &snap.in_flight[0];
        assert_eq!(up.attempt, 1);
        // first_started falls back to current-attempt start when missing
        assert_eq!(up.first_started_unix, up.started_at_unix);
        assert_eq!(up.bytes_uploaded, None,
            "no progress entry → bytes_uploaded is None, not Some(0)");
    }

    #[tokio::test]
    async fn snapshot_reports_zero_bytes_when_progress_is_explicitly_zero() {
        // Distinguishes "we just inserted the progress entry, no bytes
        // moved yet" from "no progress reporting at all (sub-second
        // upload, mock backend, etc)."
        let (db, _mid, inode) = make_db_with_inode("/x.txt", 100).await;
        let mut started = HashMap::new();
        started.insert(inode, Instant::now());
        let progress: Arc<DashMap<Inode, u64>> = Arc::new(DashMap::new());
        progress.insert(inode, 0);

        let snap = build_queue_snapshot(
            &HashMap::new(), &started,
            &HashMap::new(), &HashMap::new(),
            &progress, &db,
        ).await;
        assert_eq!(snap.in_flight[0].bytes_uploaded, Some(0));
    }
}
