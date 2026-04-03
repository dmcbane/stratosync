//! stratosyncd — startup, crash recovery, per-mount orchestration, signal handling.
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use tracing::{error, info, warn};
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

mod cache;
mod config_io;
mod fuse;
mod sync;
mod watcher;

use stratosync_core::{
    backend::RcloneBackend,
    config::{default_data_dir, Config},
    state::StateDb,
    Backend,
};
use sync::{RemotePoller, UploadQueue};

#[tokio::main]
async fn main() -> Result<()> {
    let config_path = std::env::var("STRATOSYNC_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| stratosync_core::config::default_config_path());

    let cfg = config_io::load(&config_path)
        .with_context(|| format!("load config {config_path:?}"))?;

    init_logging(&cfg);
    info!(version = env!("CARGO_PKG_VERSION"), "stratosyncd starting");

    let db_path = default_data_dir().join("state.db");
    let db = Arc::new(StateDb::open(&db_path)?);
    db.migrate().await.context("DB migrations")?;

    // Crash recovery
    let reset = db.reset_hydrating().await?;
    if reset > 0 { info!(count = reset, "reset stale hydrations"); }

    let mut fuse_threads = vec![];
    let mut _watchers = vec![]; // must outlive fuse_threads to keep inotify alive

    for mount_cfg in cfg.mounts.iter().filter(|m| m.enabled) {
        let backend: Arc<dyn Backend> = Arc::new(
            RcloneBackend::new(&mount_cfg.remote)
                .with_context(|| format!("backend for {:?}", mount_cfg.name))?,
        );

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

        // Re-queue any dirty/uploading files from prior run
        let pending = db.get_pending_uploads(mount_id).await?;
        if !pending.is_empty() {
            warn!(count = pending.len(), mount = %mount_cfg.name, "re-queuing pending uploads");
        }

        // Upload queue
        let upload_queue = Arc::new(UploadQueue::new(
            mount_id, Arc::clone(&db), Arc::clone(&backend),
            mount_cfg.poll_duration()?,               // reuse poll interval as debounce base
            std::time::Duration::from_millis(
                cfg.daemon.sync.upload_close_debounce_ms),
            cfg.daemon.sync.max_upload_concurrent,
        ));

        for entry in pending {
            upload_queue.enqueue(sync::UploadTrigger::Write { inode: entry.inode }).await;
        }

        // Ensure cache directory exists before watcher and FUSE mount need it.
        // fuse::mount() creates .meta/partial inside it, but the watcher needs
        // the base directory to exist first.
        let cache_dir = mount_cfg.cache_dir();
        std::fs::create_dir_all(&cache_dir)
            .with_context(|| format!("create cache dir {:?}", cache_dir))?;

        // Cache manager
        cache::CacheManager::new(mount_id, Arc::clone(&db), mount_cfg.cache_quota_bytes()?)
            .with_marks(
                mount_cfg.eviction.low_mark,
                mount_cfg.eviction.high_mark,
            )
            .spawn();

        // inotify watcher — must be stored to keep the watcher alive
        match watcher::FsWatcher::start(
            mount_cfg.cache_dir(), mount_id,
            Arc::clone(&db), Arc::clone(&upload_queue),
        ) {
            Ok(w) => _watchers.push(w),
            Err(e) => warn!(mount = %mount_cfg.name, "inotify watcher failed to start: {e}"),
        }

        // Remote poller
        let poller = RemotePoller::new(
            mount_id, Arc::clone(&db), Arc::clone(&backend),
            mount_cfg.poll_duration()?,
        );
        let poller_mount = mount_cfg.name.clone();
        tokio::spawn(async move {
            poller.run().await;
            error!(mount = %poller_mount, "remote poller exited unexpectedly");
        });

        // FUSE mount (blocking thread)
        let mount_path   = mount_cfg.resolved_mount_path();
        let fuse_cfg     = cfg.daemon.fuse.clone();
        let mount_name   = mount_cfg.name.clone();
        let db_c         = Arc::clone(&db);
        let backend_c    = Arc::clone(&backend);
        let queue_c      = Arc::clone(&upload_queue);
        // Capture the tokio Handle here (main thread has runtime context).
        // The spawned std::thread has no tokio context, so Handle::current()
        // would panic if called from within it.
        let rt_handle    = tokio::runtime::Handle::current();

        let handle = std::thread::Builder::new()
            .name(format!("fuse-{}", mount_name))
            .spawn(move || {
                if let Err(e) = fuse::mount(
                    &mount_name, mount_id, &mount_path,
                    cache_dir, db_c, backend_c, queue_c, fuse_cfg, rt_handle,
                ) {
                    error!(mount = %mount_name, "FUSE error: {e}");
                }
            })?;

        fuse_threads.push(handle);
    }

    if fuse_threads.is_empty() {
        anyhow::bail!("no enabled mounts in {config_path:?}");
    }

    tokio::signal::ctrl_c().await?;
    info!("shutdown signal — stopping");
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
