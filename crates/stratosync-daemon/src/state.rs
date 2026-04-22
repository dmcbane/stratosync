//! DaemonState — aggregated per-mount handles for the dashboard IPC.
//!
//! Built once during `main.rs` mount setup and handed to the IPC server.
//! `snapshot()` walks every mount and produces a `DaemonStatus` suitable
//! for serialization over the socket.
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use dashmap::DashMap;
use libc::c_int;
use tokio::sync::{oneshot, RwLock};

use stratosync_core::{
    ipc::{CacheStatus, DaemonStatus, HydrationStatus, MountStatus, PollerStatus},
    state::StateDb,
    types::{Inode, SyncStatus},
};

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

async fn collect_mount_status(m: &MountHandle) -> MountStatus {
    let cache = CacheStatus {
        used_bytes:   m.db.total_cache_bytes(m.mount_id).await.unwrap_or(0),
        quota_bytes:  m.quota_bytes,
        pinned_count: m.db.pinned_count(m.mount_id).await.unwrap_or(0),
    };

    let queue = m.upload_queue.snapshot().await;

    let poller = m.poller_state.read().await.clone();

    let hydration_active = m.db
        .count_by_status(m.mount_id, SyncStatus::Hydrating).await
        .unwrap_or(0);
    let hydration_waiters: u64 = m.hydration_waiters.iter()
        .map(|r| r.value().len() as u64)
        .sum();
    let hydration = HydrationStatus {
        active:  hydration_active,
        waiters: hydration_waiters,
    };

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
