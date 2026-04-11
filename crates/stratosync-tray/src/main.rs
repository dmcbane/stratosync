/// stratosync-tray — system tray indicator for stratosync.
///
/// Polls mount databases every few seconds and displays sync status
/// in the system tray via the StatusNotifierItem (SNI) protocol.
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use bytesize::ByteSize;
use stratosync_core::config::{default_config_path, default_data_dir};
use tracing_subscriber::EnvFilter;

// ── Mount status snapshot ────────────────────────────────────────────────────

#[derive(Clone, Default)]
struct MountStatus {
    name:       String,
    enabled:    bool,
    cache_used: u64,
    cache_quota: u64,
    syncing:    u64,   // dirty + uploading count
    conflicts:  u64,
    pinned:     u64,
    mounted:    bool,
}

#[derive(Clone, Default)]
struct GlobalStatus {
    mounts: Vec<MountStatus>,
}

impl GlobalStatus {
    fn icon_name(&self) -> &'static str {
        if self.mounts.iter().any(|m| m.conflicts > 0) {
            "dialog-warning"
        } else if self.mounts.iter().any(|m| m.syncing > 0) {
            "sync-synchronizing"
        } else if self.mounts.iter().any(|m| m.mounted) {
            "folder-cloud"
        } else {
            "cloud-offline"
        }
    }

    fn tooltip(&self) -> String {
        let total_syncing: u64 = self.mounts.iter().map(|m| m.syncing).sum();
        let total_conflicts: u64 = self.mounts.iter().map(|m| m.conflicts).sum();

        if total_conflicts > 0 {
            format!("stratosync: {} conflict(s)", total_conflicts)
        } else if total_syncing > 0 {
            format!("stratosync: syncing {} file(s)", total_syncing)
        } else {
            "stratosync: idle".into()
        }
    }
}

// ── Polling ──────────────────────────────────────────────────────────────────

fn poll_status() -> GlobalStatus {
    let config_path = default_config_path();
    let Ok(cfg) = load_config(&config_path) else {
        return GlobalStatus::default();
    };

    let mut mounts = Vec::new();
    for mount in cfg.mounts.iter().filter(|m| m.enabled) {
        let ms = poll_mount(mount);
        mounts.push(ms);
    }

    GlobalStatus { mounts }
}

fn poll_mount(mount: &stratosync_core::config::MountConfig) -> MountStatus {
    let db_path = default_data_dir().join(format!("{}.db", mount.name));
    let mut ms = MountStatus {
        name: mount.name.clone(),
        enabled: mount.enabled,
        cache_quota: mount.cache_quota_bytes().unwrap_or(0),
        ..Default::default()
    };

    // Check if FUSE mount is active
    let mount_path = expand_tilde(&mount.resolved_mount_path());
    ms.mounted = is_fuse_mounted(&mount_path);

    if !db_path.exists() { return ms; }

    let Ok(conn) = rusqlite::Connection::open_with_flags(
        &db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
            | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) else { return ms };

    let mount_id: u32 = conn.query_row(
        "SELECT id FROM mounts WHERE name = ?1",
        rusqlite::params![mount.name],
        |r| r.get(0),
    ).unwrap_or(0);
    if mount_id == 0 { return ms; }

    // Cache usage
    ms.cache_used = conn.query_row(
        "SELECT COALESCE(SUM(cache_size), 0) FROM file_index WHERE mount_id=?1 AND cache_size IS NOT NULL",
        rusqlite::params![mount_id],
        |r| r.get::<_, i64>(0),
    ).unwrap_or(0) as u64;

    // Syncing count (dirty + uploading)
    ms.syncing = conn.query_row(
        "SELECT COUNT(*) FROM file_index WHERE mount_id=?1 AND status IN ('dirty','uploading')",
        rusqlite::params![mount_id],
        |r| r.get::<_, i64>(0),
    ).unwrap_or(0) as u64;

    // Conflict count
    ms.conflicts = conn.query_row(
        "SELECT COUNT(*) FROM file_index WHERE mount_id=?1 AND (status='conflict' OR name LIKE '%.conflict.%')",
        rusqlite::params![mount_id],
        |r| r.get::<_, i64>(0),
    ).unwrap_or(0) as u64;

    // Pinned count
    ms.pinned = conn.query_row(
        "SELECT COUNT(*) FROM cache_lru l JOIN file_index f ON l.inode=f.inode WHERE f.mount_id=?1 AND l.pinned=1",
        rusqlite::params![mount_id],
        |r| r.get::<_, i64>(0),
    ).unwrap_or(0) as u64;

    ms
}

fn is_fuse_mounted(path: &std::path::Path) -> bool {
    let Ok(content) = std::fs::read_to_string("/proc/mounts") else { return false };
    let path_str = path.to_string_lossy();
    content.lines().any(|line| {
        line.contains("fuse") && line.contains(&*path_str)
    })
}

fn expand_tilde(p: &std::path::Path) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    p.to_owned()
}

fn load_config(path: &std::path::Path) -> Result<stratosync_core::Config> {
    let src = std::fs::read_to_string(path)?;
    let cfg: stratosync_core::Config = toml::from_str(&src)?;
    Ok(cfg)
}

// ── KSNI tray implementation ─────────────────────────────────────────────────

struct StratoSyncTray {
    status: Arc<Mutex<GlobalStatus>>,
}

impl ksni::Tray for StratoSyncTray {
    fn id(&self) -> String {
        "stratosync".into()
    }

    fn icon_name(&self) -> String {
        let status = self.status.lock().unwrap();
        status.icon_name().into()
    }

    fn title(&self) -> String {
        "stratosync".into()
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        let status = self.status.lock().unwrap();
        ksni::ToolTip {
            title: status.tooltip(),
            description: String::new(),
            icon_name: String::new(),
            icon_pixmap: Vec::new(),
        }
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        let status = self.status.lock().unwrap();
        let mut items: Vec<ksni::MenuItem<Self>> = Vec::new();

        for mount in &status.mounts {
            let label = if mount.syncing > 0 {
                format!("{}: {} cached, {} syncing",
                    mount.name,
                    ByteSize(mount.cache_used),
                    mount.syncing)
            } else {
                format!("{}: {} cached",
                    mount.name,
                    ByteSize(mount.cache_used))
            };

            items.push(ksni::MenuItem::Standard(ksni::menu::StandardItem {
                label,
                enabled: false,
                ..Default::default()
            }));

            if mount.conflicts > 0 {
                items.push(ksni::MenuItem::Standard(ksni::menu::StandardItem {
                    label: format!("  {} conflict(s)", mount.conflicts),
                    enabled: false,
                    ..Default::default()
                }));
            }
            if mount.pinned > 0 {
                items.push(ksni::MenuItem::Standard(ksni::menu::StandardItem {
                    label: format!("  {} pinned", mount.pinned),
                    enabled: false,
                    ..Default::default()
                }));
            }
        }

        items.push(ksni::MenuItem::Separator);

        items.push(ksni::MenuItem::Standard(ksni::menu::StandardItem {
            label: "Quit".into(),
            activate: Box::new(|_| std::process::exit(0)),
            ..Default::default()
        }));

        items
    }
}

// ── Main ─────────────────────────────────────────────────────────────────────

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_target(false)
        .init();

    let status = Arc::new(Mutex::new(poll_status()));

    // Background polling thread
    let status_bg = Arc::clone(&status);
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(std::time::Duration::from_secs(5));
            let new_status = poll_status();
            *status_bg.lock().unwrap() = new_status;
        }
    });

    use ksni::blocking::TrayMethods;
    let tray = StratoSyncTray { status };
    match tray.spawn() {
        Ok(handle) => {
            // Block until the tray is shut down
            handle.shutdown().wait();
        }
        Err(e) => {
            eprintln!("Failed to start tray indicator: {e}");
            eprintln!("Ensure a StatusNotifierItem host is running (KDE, GNOME with extension, etc.)");
            std::process::exit(1);
        }
    }
}
