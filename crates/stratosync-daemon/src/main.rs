//! stratosyncd — startup, crash recovery, per-mount orchestration, signal handling.
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use tracing::{error, info, warn};
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

mod cache;
mod config_io;
mod fuse;
mod metrics;
mod server;
mod state;
mod sync;
mod watcher;
mod webdav;

use dashmap::DashMap;
use stratosync_core::{
    backend::RcloneBackend,
    base_store::BaseStore,
    config::{default_data_dir, default_runtime_socket, Config},
    state::StateDb,
    Backend,
};
use sync::{RemotePoller, UploadQueue};

use state::{DaemonState, MountHandle};

#[tokio::main]
async fn main() -> Result<()> {
    let config_path = std::env::var("STRATOSYNC_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| stratosync_core::config::default_config_path());

    let cfg = config_io::load(&config_path)
        .with_context(|| format!("load config {config_path:?}"))?;

    init_logging(&cfg);
    info!(version = env!("CARGO_PKG_VERSION"), "stratosyncd starting");

    let mut fuse_threads = vec![];
    let mut mount_paths = vec![];
    let mut _watchers = vec![]; // must outlive fuse_threads to keep inotify alive
    let mut sidecars: Vec<webdav::WebDavSidecar> = vec![];
    let mut mount_id_counter = 0u32;
    let mut mount_handles: Vec<MountHandle> = Vec::new();

    for mount_cfg in cfg.mounts.iter().filter(|m| m.enabled) {
        mount_id_counter += 1;
        // Per-mount database — each mount gets its own SQLite file so inodes
        // and file entries are fully isolated.  This prevents the bug where
        // two mounts sharing a single DB would return cross-mount children
        // (the root inode 1 collision + unfiltered list_children).
        let db_path = default_data_dir().join(format!("{}.db", mount_cfg.name));
        let db = Arc::new(StateDb::open(&db_path)?);
        db.migrate().await
            .with_context(|| format!("DB migration for mount {:?}", mount_cfg.name))?;

        // Crash recovery (per-mount)
        let reset = db.reset_hydrating().await?;
        if reset > 0 { info!(count = reset, mount = %mount_cfg.name, "reset stale hydrations"); }

        let backend: Arc<dyn Backend> = if cfg.daemon.webdav_sidecar {
            let sidecar = webdav::WebDavSidecar::start(
                &RcloneBackend::which_rclone()?,
                &mount_cfg.remote,
                mount_id_counter,
            ).await.with_context(|| format!("WebDAV sidecar for {:?}", mount_cfg.name))?;
            sidecars.push(sidecar);
            Arc::new(stratosync_core::WebDavBackend::new(
                sidecars.last().unwrap().base_url(),
            ))
        } else {
            let mut rclone_backend = RcloneBackend::new(&mount_cfg.remote)
                .with_context(|| format!("backend for {:?}", mount_cfg.name))?;
            rclone_backend.init_delta().await;
            Arc::new(rclone_backend)
        };

        let mount_id = db.upsert_mount(
            &mount_cfg.name, &mount_cfg.remote,
            &mount_cfg.mount_path.to_string_lossy(),
            &mount_cfg.cache_dir().to_string_lossy(),
            mount_cfg.cache_quota_bytes()?,
            mount_cfg.poll_duration()?.as_secs() as u32,
        ).await?;

        info!(name = %mount_cfg.name, mount = %mount_cfg.mount_path.display(), "mounting");

        // Ensure root directory entry exists BEFORE poller or FUSE start.
        // The poller inserts files with parent=FUSE_ROOT_INODE, so inode 1
        // must exist first (FK constraint). Also validates that any existing
        // inode 1 is actually the root directory.
        fuse::ensure_root(&db, mount_id).await?;

        // Ensure cache directory exists before watcher and FUSE mount need it.
        // fuse::mount() creates .meta/partial inside it, but the watcher needs
        // the base directory to exist first.
        let cache_dir = mount_cfg.cache_dir();
        std::fs::create_dir_all(&cache_dir)
            .with_context(|| format!("create cache dir {:?}", cache_dir))?;

        // Base-version object store (for 3-way merge conflict resolution)
        let base_store = Arc::new(
            BaseStore::new(cache_dir.join(".bases"))
                .with_context(|| format!("create base store for mount {:?}", mount_cfg.name))?
        );
        let sync_config = Arc::new(cfg.daemon.sync.clone());

        // Selective sync: compile ignore patterns once, share the matcher.
        // Fail fast on bad patterns so users see the error at startup.
        let ignore = Arc::new(
            mount_cfg.build_ignore_set()
                .with_context(|| format!("ignore patterns for mount {:?}", mount_cfg.name))?
        );
        if !mount_cfg.ignore_patterns.is_empty() {
            info!(
                mount = %mount_cfg.name,
                count = mount_cfg.ignore_patterns.len(),
                "selective sync enabled"
            );
        }

        // Bandwidth schedule: parse the optional upload_window once at
        // startup. Bad windows abort startup with a clear error.
        let upload_window = mount_cfg.parse_upload_window()
            .with_context(|| format!("upload_window for mount {:?}", mount_cfg.name))?;
        if let Some(w) = upload_window {
            info!(
                mount = %mount_cfg.name,
                start = format!("{:02}:{:02}", w.start_min / 60, w.start_min % 60),
                end   = format!("{:02}:{:02}", w.end_min   / 60, w.end_min   % 60),
                "bandwidth schedule active"
            );
        }

        // Re-queue any dirty/uploading files from prior run
        let pending = db.get_pending_uploads(mount_id).await?;
        if !pending.is_empty() {
            warn!(count = pending.len(), mount = %mount_cfg.name, "re-queuing pending uploads");
        }

        // Upload queue
        let upload_queue = Arc::new(UploadQueue::new(
            mount_id, Arc::clone(&db), Arc::clone(&backend),
            Arc::clone(&base_store), Arc::clone(&sync_config),
            mount_cfg.poll_duration()?,               // reuse poll interval as debounce base
            std::time::Duration::from_millis(
                cfg.daemon.sync.upload_close_debounce_ms),
            cfg.daemon.sync.max_upload_concurrent,
            upload_window,
            mount_cfg.version_retention,
        ));

        for entry in pending {
            upload_queue.enqueue(sync::UploadTrigger::Write { inode: entry.inode }).await;
        }

        // Cache manager
        cache::CacheManager::new(mount_id, Arc::clone(&db), mount_cfg.cache_quota_bytes()?)
            .with_marks(
                mount_cfg.eviction.low_mark,
                mount_cfg.eviction.high_mark,
            )
            .spawn();

        // Base version eviction (stale base objects for 3-way merge)
        cache::spawn_base_eviction(
            mount_id, Arc::clone(&db), Arc::clone(&base_store),
            cfg.daemon.sync.base_retention_days,
        );

        // inotify watcher — must be stored to keep the watcher alive
        match watcher::FsWatcher::start(
            mount_cfg.cache_dir(), mount_id,
            Arc::clone(&db), Arc::clone(&upload_queue),
            Arc::clone(&ignore),
        ) {
            Ok(w) => _watchers.push(w),
            Err(e) => warn!(mount = %mount_cfg.name, "inotify watcher failed to start: {e}"),
        }

        // Remote poller
        let version_max_size = cfg.daemon.sync.base_max_file_size_bytes()
            .unwrap_or(10 * 1024 * 1024);
        let poller = RemotePoller::new(
            mount_id, Arc::clone(&db), Arc::clone(&backend),
            mount_cfg.poll_duration()?,
            Arc::clone(&ignore),
            Arc::clone(&base_store),
            version_max_size,
            mount_cfg.version_retention,
        );
        let poller_state = poller.state_handle();
        let poller_mount = mount_cfg.name.clone();
        tokio::spawn(async move {
            poller.run().await;
            error!(mount = %poller_mount, "remote poller exited unexpectedly");
        });

        // Hydration waiters — created up-front so both FUSE and the dashboard
        // share the same DashMap.
        let hydration_waiters = Arc::new(DashMap::new());

        // Collect a handle for the dashboard IPC aggregator.
        mount_handles.push(MountHandle {
            name:              mount_cfg.name.clone(),
            remote:            mount_cfg.remote.clone(),
            mount_path:        mount_cfg.mount_path.to_string_lossy().into_owned(),
            quota_bytes:       mount_cfg.cache_quota_bytes()?,
            mount_id,
            db:                Arc::clone(&db),
            upload_queue:      Arc::clone(&upload_queue),
            poller_state,
            hydration_waiters: Arc::clone(&hydration_waiters),
        });

        // FUSE mount (blocking thread)
        let mount_path   = mount_cfg.resolved_mount_path();
        let fuse_cfg     = cfg.daemon.fuse.clone();
        let mount_name   = mount_cfg.name.clone();
        let db_c         = Arc::clone(&db);
        let backend_c    = Arc::clone(&backend);
        let queue_c      = Arc::clone(&upload_queue);
        let base_store_c = Arc::clone(&base_store);
        let sync_cfg_c   = Arc::clone(&sync_config);
        let waiters_c    = Arc::clone(&hydration_waiters);
        let ignore_c     = Arc::clone(&ignore);
        // Capture the tokio Handle here (main thread has runtime context).
        // The spawned std::thread has no tokio context, so Handle::current()
        // would panic if called from within it.
        let rt_handle    = tokio::runtime::Handle::current();

        let handle = std::thread::Builder::new()
            .name(format!("fuse-{}", mount_name))
            .spawn(move || {
                if let Err(e) = fuse::mount(
                    &mount_name, mount_id, &mount_path,
                    cache_dir, db_c, backend_c, queue_c,
                    base_store_c, sync_cfg_c, fuse_cfg, rt_handle,
                    waiters_c, ignore_c,
                ) {
                    error!(mount = %mount_name, "FUSE error: {e}");
                }
            })?;

        fuse_threads.push(handle);
        mount_paths.push(mount_cfg.resolved_mount_path());
    }

    if fuse_threads.is_empty() {
        anyhow::bail!("no enabled mounts in {config_path:?}");
    }

    // Dashboard IPC server — serves DaemonStatus over a Unix socket so
    // the `stratosync dashboard` CLI can visualize what's happening.
    let socket_path = cfg.daemon.socket.clone().unwrap_or_else(default_runtime_socket);
    let daemon_state = Arc::new(DaemonState::new(mount_handles));
    let server_socket = socket_path.clone();
    let server_state = Arc::clone(&daemon_state);
    tokio::spawn(async move {
        if let Err(e) = server::serve(server_socket, server_state).await {
            error!("IPC server exited: {e}");
        }
    });

    // Optional Prometheus metrics endpoint — opt-in via `[daemon.metrics]
    // enabled = true`. Failure to bind is logged but doesn't take down the
    // daemon (operator's port choice is their problem to fix).
    if cfg.daemon.metrics.enabled {
        let metrics_addr  = cfg.daemon.metrics.listen_addr.clone();
        let metrics_state = Arc::clone(&daemon_state);
        tokio::spawn(async move {
            if let Err(e) = metrics::serve(metrics_addr, metrics_state).await {
                error!("metrics endpoint exited: {e}");
            }
        });
    }

    tokio::signal::ctrl_c().await?;
    info!("shutdown signal — unmounting");

    // Remove the IPC socket so the next startup gets a clean bind.
    let _ = std::fs::remove_file(&socket_path);

    // Stop WebDAV sidecars
    for mut sidecar in sidecars {
        sidecar.stop().await;
    }
    for path in &mount_paths {
        info!(path = %path.display(), "unmounting");
        // fusermount3 for FUSE3, fusermount for FUSE2, umount as fallback
        let unmounted = std::process::Command::new("fusermount3")
            .args(["-u", &path.to_string_lossy()])
            .status()
            .ok()
            .map_or(false, |s| s.success())
        || std::process::Command::new("fusermount")
            .args(["-u", &path.to_string_lossy()])
            .status()
            .ok()
            .map_or(false, |s| s.success());
        if !unmounted {
            warn!(path = %path.display(), "unmount failed — FUSE thread may hang");
        }
    }
    info!("waiting for FUSE threads");
    for h in fuse_threads {
        if let Err(panic_val) = h.join() {
            let msg = panic_val.downcast_ref::<String>()
                .map(|s| s.as_str())
                .or_else(|| panic_val.downcast_ref::<&str>().copied())
                .unwrap_or("unknown panic");
            error!("FUSE thread panicked: {msg}");
        }
    }
    info!("stratosyncd stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    /// Proves the bug: Handle::current() panics when called from a bare
    /// std::thread (no tokio runtime context). The fix is to capture the
    /// Handle in the tokio context and pass it into the thread.
    #[tokio::test]
    async fn handle_current_panics_on_bare_thread() {
        let h = std::thread::spawn(|| {
            std::panic::catch_unwind(tokio::runtime::Handle::current)
        });
        let result = h.join().expect("thread itself must not panic");
        assert!(result.is_err(), "Handle::current() must panic outside tokio context");
    }

    /// The correct pattern: capture the Handle in a tokio context, then
    /// use it from a spawned std::thread via block_on.
    #[tokio::test]
    async fn captured_handle_works_on_bare_thread() {
        let rt = tokio::runtime::Handle::current();
        let h = std::thread::spawn(move || {
            rt.block_on(async { 42 })
        });
        let result = h.join().expect("thread must not panic");
        assert_eq!(result, 42);
    }
}

fn init_logging(cfg: &Config) {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(cfg.daemon.log_level.as_str()));
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_target(true).with_thread_names(true))
        .init();
}
