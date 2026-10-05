//! DaemonState — aggregated per-mount handles for the dashboard IPC.
//!
//! Built once during `main.rs` mount setup and handed to the IPC server.
//! `snapshot()` walks every mount and produces a `DaemonStatus` suitable
//! for serialization over the socket.
use std::sync::Arc;
use std::future::Future;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use dashmap::DashMap;
use libc::c_int;
use tokio::sync::{oneshot, RwLock};

use stratosync_core::{
    ipc::{CacheStatus, DaemonStatus, HydrationStatus, MountStatus, PollerStatus, QueueStatus},
    state::StateDb,
    types::{Inode, SyncStatus},
};

use crate::fuse::HydrationTracker;
use crate::sync::UploadQueue;

/// A single mount's live handles. Cheap to clone (all Arcs).
pub struct MountHandle {
    pub name:              String,
    pub remote:            String,
    pub mount_path:        String,
    pub quota_bytes:       u64,
    pub mount_id:          u32,
    pub db:                Arc<StateDb>,
    pub upload_queue:      Arc<UploadQueue>,
    pub poller_state:      Arc<RwLock<PollerStatus>>,
    pub hydration_waiters: Arc<DashMap<Inode, Vec<oneshot::Sender<Result<(), c_int>>>>>,
    pub hydration_tracker: HydrationTracker,
}

pub struct DaemonState {
    pub start_time: Instant,
    pub mounts:     Vec<MountHandle>,
}

impl DaemonState {
    pub fn new(mounts: Vec<MountHandle>) -> Self {
        Self { start_time: Instant::now(), mounts }
    }

    /// Collect a full status snapshot for every mount.
    pub async fn snapshot(&self) -> DaemonStatus {
        let mut mounts = Vec::with_capacity(self.mounts.len());
        for m in &self.mounts {
            mounts.push(collect_mount_status(m).await);
        }
        DaemonStatus {
            version:     env!("CARGO_PKG_VERSION").to_string(),
            pid:         std::process::id(),
            uptime_secs: self.start_time.elapsed().as_secs(),
            mounts,
        }
    }
}

/// How long `status` waits for the upload loop to answer a snapshot.
/// The loop runs conflict resolution inline, so it can be busy for as
/// long as a full download takes; status must not inherit that latency.
const QUEUE_SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(1);

async fn queue_snapshot_or_busy(
    snapshot: impl Future<Output = QueueStatus>, limit: Duration,
) -> QueueStatus {
    match tokio::time::timeout(limit, snapshot).await {
        Ok(s) => s,
        Err(_) => {
            tracing::debug!(?limit, "upload queue snapshot timed out — reporting busy");
            QueueStatus { busy: true, ..Default::default() }
        }
    }
}

async fn collect_mount_status(m: &MountHandle) -> MountStatus {
    let cache = CacheStatus {
        used_bytes:   m.db.total_cache_bytes(m.mount_id).await.unwrap_or(0),
        quota_bytes:  m.quota_bytes,
        pinned_count: m.db.pinned_count(m.mount_id).await.unwrap_or(0),
    };

    let mut queue = queue_snapshot_or_busy(
        m.upload_queue.snapshot(), QUEUE_SNAPSHOT_TIMEOUT,
    ).await;

    let poller = m.poller_state.read().await.clone();

    let hydration_active = m.db
        .count_by_status(m.mount_id, SyncStatus::Hydrating).await
        .unwrap_or(0);
    let hydration_waiters: u64 = m.hydration_waiters.iter()
        .map(|r| r.value().len() as u64)
        .sum();
    let health = m.db.get_mount_health(m.mount_id).await.unwrap_or_default();
    let hydration = HydrationStatus {
        active:               hydration_active,
        waiters:              hydration_waiters,
        consecutive_failures: health.consecutive_hydration_failures,
        last_error:           health.last_hydration_error,
        last_failure_unix:    health.last_hydration_failure_unix,
        in_flight:            m.hydration_tracker.snapshot(),
    };
    // Inject upload health into the queue snapshot — the upload queue
    // doesn't see the DB row itself, so we splice it in here right next
    // to the hydration twin so the same get_mount_health call serves
    // both directions.
    queue.consecutive_failures = health.consecutive_upload_failures;
    queue.last_error           = health.last_upload_error;
    queue.last_failure_unix    = health.last_upload_failure_unix;

    let conflicts = m.db.count_conflicts(m.mount_id).await.unwrap_or(0);

    MountStatus {
        name:       m.name.clone(),
        remote:     m.remote.clone(),
        mount_path: m.mount_path.clone(),
        cache, queue, poller, hydration, conflicts,
    }
}

/// Convenience: current unix-epoch seconds.
#[allow(dead_code)]
pub fn now_unix() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// The queue snapshot travels through the upload loop's trigger
    /// channel, and that loop runs conflict resolution inline. While it
    /// is busy the snapshot never answers — and the whole `status` IPC
    /// (dashboard, tray) used to hang with it. A busy queue must
    /// produce a bounded, explicitly flagged reply instead.
    #[tokio::test]
    async fn queue_snapshot_times_out_as_busy() {
        let s = queue_snapshot_or_busy(std::future::pending(), Duration::from_millis(50)).await;
        assert!(s.busy, "unanswered snapshot must be reported as busy, not hang");
    }

    #[tokio::test]
    async fn queue_snapshot_passes_through_when_answered() {
        let ready = async { QueueStatus { pending: 7, ..Default::default() } };
        let s = queue_snapshot_or_busy(ready, Duration::from_secs(1)).await;
        assert!(!s.busy);
        assert_eq!(s.pending, 7);
    }
}
