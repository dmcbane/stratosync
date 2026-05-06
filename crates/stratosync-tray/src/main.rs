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

/// Number of consecutive hydration failures at which the tray flips into
/// the "downloads stalled" warning state. Single transient errors are
/// frequent on flaky networks and would just cause icon flicker; a small
/// run of failures is what users actually want to know about.
const HYDRATION_DEGRADED_THRESHOLD: u32 = 3;

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
    /// Consecutive hydration (download) failures since the last success.
    /// Sourced from the `mount_health` table written by the daemon.
    hydration_failures: u32,
    /// Most recent hydration error message, kept across recoveries so the
    /// menu can still show "last download error: …" after things settle.
    last_hydration_error: Option<String>,
}

impl MountStatus {
    fn is_degraded(&self) -> bool {
        self.hydration_failures >= HYDRATION_DEGRADED_THRESHOLD
    }
}

#[derive(Clone, Default)]
struct GlobalStatus {
    mounts: Vec<MountStatus>,
}

impl GlobalStatus {
    fn icon_name(&self) -> &'static str {
        // Order matters: a "stalled download" or active conflict is louder
        // than a normal in-flight sync. Conflicts win over hydration so a
        // user with both still gets the conflict-aware menu first.
        if self.mounts.iter().any(|m| m.conflicts > 0) {
            "dialog-warning"
        } else if self.mounts.iter().any(|m| m.is_degraded()) {
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
        let stalled: Vec<&MountStatus> = self.mounts.iter()
            .filter(|m| m.is_degraded()).collect();

        if total_conflicts > 0 {
            format!("stratosync: {} conflict(s)", total_conflicts)
        } else if !stalled.is_empty() {
            // One mount stalled is the common case — name it. Multi-mount
            // stalls roll up to a count to keep the tooltip short.
            if stalled.len() == 1 {
                format!("stratosync: download stalled on {} ({} fail(s))",
                    stalled[0].name, stalled[0].hydration_failures)
            } else {
                format!("stratosync: downloads stalled on {} mount(s)", stalled.len())
            }
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

    // Hydration health — written by the daemon's FUSE layer. Absent on a
    // freshly migrated DB that has never had a hydration; treat that as
    // healthy (zero failures).
    if let Ok((failures, last_error)) = conn.query_row(
        "SELECT consecutive_hydration_failures, last_hydration_error
         FROM mount_health WHERE mount_id = ?1",
        rusqlite::params![mount_id],
        |r| Ok((r.get::<_, i64>(0)? as u32, r.get::<_, Option<String>>(1)?)),
    ) {
        ms.hydration_failures = failures;
        ms.last_hydration_error = last_error;
    }

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
            if mount.is_degraded() {
                let err = mount.last_hydration_error.as_deref()
                    .unwrap_or("(unknown error)");
                // Truncate long errors so the menu stays readable; the
                // dashboard / logs are the authoritative place for full
                // diagnostics.
                let short: String = err.chars().take(80).collect();
                items.push(ksni::MenuItem::Standard(ksni::menu::StandardItem {
                    label: format!("  download stalled ({} fail(s)): {}",
                        mount.hydration_failures, short),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn mount(name: &str) -> MountStatus {
        MountStatus { name: name.into(), enabled: true, mounted: true, ..Default::default() }
    }

    #[test]
    fn idle_mount_shows_folder_cloud() {
        let g = GlobalStatus { mounts: vec![mount("a")] };
        assert_eq!(g.icon_name(), "folder-cloud");
    }

    #[test]
    fn one_or_two_failures_do_not_flip_to_degraded() {
        let mut m = mount("a");
        m.hydration_failures = HYDRATION_DEGRADED_THRESHOLD - 1;
        let g = GlobalStatus { mounts: vec![m] };
        assert_eq!(g.icon_name(), "folder-cloud",
            "single transient failures should not warn the user");
        assert_eq!(g.tooltip(), "stratosync: idle");
    }

    #[test]
    fn hydration_failures_at_threshold_warn() {
        let mut m = mount("gdrive");
        m.hydration_failures = HYDRATION_DEGRADED_THRESHOLD;
        m.last_hydration_error = Some("not found: ino=2179".into());
        let g = GlobalStatus { mounts: vec![m] };
        assert_eq!(g.icon_name(), "dialog-warning");
        let tip = g.tooltip();
        assert!(tip.contains("gdrive"), "tooltip names the stalled mount: {tip}");
        assert!(tip.contains("stalled"));
    }

    #[test]
    fn conflicts_take_priority_over_hydration_warning() {
        let mut m = mount("a");
        m.hydration_failures = HYDRATION_DEGRADED_THRESHOLD;
        m.conflicts = 2;
        let g = GlobalStatus { mounts: vec![m] };
        assert_eq!(g.icon_name(), "dialog-warning");
        // Conflicts win the tooltip — they require user action; a stalled
        // download often resolves on its own as the network recovers.
        assert!(g.tooltip().contains("conflict"));
    }

    #[test]
    fn multiple_stalled_mounts_roll_up_in_tooltip() {
        let mut a = mount("a"); a.hydration_failures = HYDRATION_DEGRADED_THRESHOLD;
        let mut b = mount("b"); b.hydration_failures = HYDRATION_DEGRADED_THRESHOLD + 5;
        let g = GlobalStatus { mounts: vec![a, b] };
        let tip = g.tooltip();
        assert!(tip.contains("2 mount"), "tip rolls up: {tip}");
    }

    #[test]
    fn syncing_overrides_idle_but_not_hydration_warning() {
        let mut m = mount("a");
        m.syncing = 5;
        m.hydration_failures = HYDRATION_DEGRADED_THRESHOLD;
        let g = GlobalStatus { mounts: vec![m] };
        // Stalled downloads outrank "syncing" — the user should see the
        // warning, not the spinner.
        assert_eq!(g.icon_name(), "dialog-warning");
    }
}
