//! Configuration types — serde-only, no file I/O.
//! File loading lives in stratosync-cli::config_io (uses the toml crate).
use std::path::PathBuf;
use std::time::Duration;
use serde::{Deserialize, Serialize};

// ── Path helpers (pure, no I/O) ───────────────────────────────────────────────

pub fn default_config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_default()))
        .join("stratosync")
        .join("config.toml")
}

pub fn default_data_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_default()))
        .join("stratosync")
}

pub fn default_cache_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_default()))
        .join("stratosync")
}

pub fn expand_tilde(path: &std::path::Path) -> PathBuf {
    let s = path.to_string_lossy();
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    path.to_owned()
}

/// Runtime socket path for the daemon IPC.
/// Prefers `$XDG_RUNTIME_DIR/stratosync.sock`, falls back to
/// `/tmp/stratosync-$UID.sock` (same uid scheme used by systemd for runtime
/// directories when XDG_RUNTIME_DIR is absent).
pub fn default_runtime_socket() -> PathBuf {
    if let Ok(rt) = std::env::var("XDG_RUNTIME_DIR") {
        if !rt.is_empty() {
            return PathBuf::from(rt).join("stratosync.sock");
        }
    }
    // SAFETY: getuid is always safe to call; returns current uid.
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/tmp/stratosync-{uid}.sock"))
}

// ── Top-level ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct Config {
    #[serde(default)]
    pub daemon: DaemonConfig,
    #[serde(rename = "mount", default)]
    pub mounts: Vec<MountConfig>,
}

// ── Daemon ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct DaemonConfig {
    pub log_level: LogLevel,
    pub log_file:  Option<PathBuf>,
    pub pid_file:  Option<PathBuf>,
    pub socket:         Option<PathBuf>,
    pub webdav_sidecar: bool,
    pub fuse:           FuseConfig,
    pub sync:           SyncConfig,
    pub metrics:        MetricsConfig,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            log_level:      LogLevel::Info,
            log_file:       None,
            pid_file:       None,
            socket:         None,
            webdav_sidecar: false,
            fuse:           FuseConfig::default(),
            sync:           SyncConfig::default(),
            metrics:        MetricsConfig::default(),
        }
    }
}

// ── Metrics ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct MetricsConfig {
    /// When true, the daemon serves a Prometheus-compatible text endpoint
    /// at `GET /metrics` on `listen_addr`. Default off — opt-in only.
    pub enabled:     bool,
    /// `host:port` to bind. Default localhost:9090.
    pub listen_addr: String,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            enabled:     false,
            listen_addr: "127.0.0.1:9090".into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel { Trace, Debug, Info, Warn, Error }

impl Default for LogLevel { fn default() -> Self { Self::Info } }

impl LogLevel {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Trace => "trace",
            Self::Debug => "debug",
            Self::Info  => "info",
            Self::Warn  => "warn",
            Self::Error => "error",
        }
    }
}

// ── FUSE ──────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct FuseConfig {
    pub threads:         usize,
    pub attr_timeout_s:  u64,
    pub entry_timeout_s: u64,
    pub allow_other:     bool,
}

impl Default for FuseConfig {
    fn default() -> Self {
        Self { threads: 4, attr_timeout_s: 60, entry_timeout_s: 60, allow_other: false }
    }
}

impl FuseConfig {
    pub fn attr_timeout(&self)  -> Duration { Duration::from_secs(self.attr_timeout_s)  }
    pub fn entry_timeout(&self) -> Duration { Duration::from_secs(self.entry_timeout_s) }
}

// ── Sync ──────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct SyncConfig {
    pub max_hydration_concurrent:  usize,
    pub max_upload_concurrent:     usize,
    pub upload_debounce_ms:        u64,
    pub upload_close_debounce_ms:  u64,
    pub text_conflict_strategy:    ConflictStrategy,
    pub base_retention_days:       u32,
    pub base_max_file_size:        String,
    pub text_extensions:           Vec<String>,
    pub prefetch_threshold:        String,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            max_hydration_concurrent:  4,
            max_upload_concurrent:     2,
            upload_debounce_ms:        2000,
            upload_close_debounce_ms:  500,
            text_conflict_strategy:    ConflictStrategy::KeepBoth,
            base_retention_days:       30,
            base_max_file_size:        "10 MB".into(),
            text_extensions:           crate::base_store::DEFAULT_TEXT_EXTENSIONS
                                           .iter().map(|s| (*s).to_string()).collect(),
            prefetch_threshold:        "1 MB".into(),
        }
    }
}

impl SyncConfig {
    pub fn upload_debounce(&self)       -> Duration { Duration::from_millis(self.upload_debounce_ms)       }
    pub fn upload_close_debounce(&self) -> Duration { Duration::from_millis(self.upload_close_debounce_ms) }
    pub fn base_max_file_size_bytes(&self) -> anyhow::Result<u64> { parse_size(&self.base_max_file_size) }
    /// Returns the prefetch size threshold in bytes, or 0 if disabled.
    pub fn prefetch_threshold_bytes(&self) -> u64 {
        parse_size(&self.prefetch_threshold).unwrap_or(0)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConflictStrategy { KeepBoth, Merge }

// ── Mount ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MountConfig {
    pub name:          String,
    pub remote:        String,
    pub mount_path:    PathBuf,
    #[serde(default = "default_cache_quota_str")]
    pub cache_quota:   String,
    #[serde(default = "default_poll_interval")]
    pub poll_interval: String,
    #[serde(default = "yes")]
    pub enabled:       bool,
    #[serde(default)]
    pub rclone:        RcloneConfig,
    #[serde(default)]
    pub eviction:      EvictionConfig,
    /// Glob patterns matched against the remote path (relative to the mount
    /// root, `/`-separated, case-sensitive). Matching entries are excluded
    /// from indexing, FUSE creates, and upload events. Already-indexed
    /// entries that newly match are preserved (not retroactively removed).
    ///
    /// Pattern semantics (via `globset` defaults): `*` matches across path
    /// separators, so `*.log` catches `foo.log`, `dir/foo.log`, and
    /// `a/b/c/foo.log` alike. Use `node_modules/**` to ignore a subtree's
    /// contents (the directory entry itself is still indexed; only its
    /// children are skipped).
    #[serde(default)]
    pub ignore_patterns: Vec<String>,
    /// Bandwidth schedule: `"HH:MM-HH:MM"` local-time window during which
    /// uploads are permitted. Outside the window, queued uploads wait
    /// until the window reopens; in-flight uploads are not interrupted.
    /// `fsync()` always bypasses the gate (user-explicit). Empty/None means
    /// uploads run any time. Wraparound is supported: `"22:00-06:00"`.
    #[serde(default)]
    pub upload_window: Option<String>,
    /// File-version retention: keep up to this many historical snapshots
    /// per file. Snapshots are captured (a) just before the poller
    /// replaces a cached file with a remote change, and (b) just after a
    /// successful upload. `0` disables versioning entirely (the default
    /// behavior prior to v0.13.0). Recover with `stratosync versions`.
    #[serde(default = "default_version_retention")]
    pub version_retention: u32,
}

fn default_version_retention() -> u32 { 10 }

fn default_cache_quota_str() -> String { "5 GiB".into()  }
fn default_poll_interval()   -> String { "60s".into()    }
fn yes() -> bool { true }

impl MountConfig {
    pub fn cache_quota_bytes(&self) -> anyhow::Result<u64>    { parse_size(&self.cache_quota)        }
    pub fn poll_duration(&self)     -> anyhow::Result<Duration> { parse_duration(&self.poll_interval) }
    pub fn resolved_mount_path(&self) -> PathBuf { expand_tilde(&self.mount_path) }
    pub fn cache_dir(&self)         -> PathBuf   { default_cache_dir().join(&self.name) }

    /// Compile `ignore_patterns` into a single matcher. Empty patterns
    /// produce an empty `GlobSet` (matches nothing).
    pub fn build_ignore_set(&self) -> anyhow::Result<globset::GlobSet> {
        let mut b = globset::GlobSetBuilder::new();
        for pat in &self.ignore_patterns {
            let g = globset::Glob::new(pat)
                .map_err(|e| anyhow::anyhow!("invalid ignore pattern {:?}: {}", pat, e))?;
            b.add(g);
        }
        b.build().map_err(|e| anyhow::anyhow!("failed to build ignore set: {}", e))
    }

    /// Parse `upload_window` if set. Returns `Ok(None)` when unset.
    pub fn parse_upload_window(&self) -> anyhow::Result<Option<UploadWindow>> {
        match self.upload_window.as_deref() {
            None | Some("") => Ok(None),
            Some(s)         => parse_upload_window(s).map(Some),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct RcloneConfig {
    pub extra_flags: Vec<String>,
    pub bwlimit:     Option<String>,
    pub transfers:   Option<u32>,
    pub checkers:    Option<u32>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct EvictionConfig {
    pub policy:    EvictionPolicy,
    pub low_mark:  f64,
    pub high_mark: f64,
}

impl Default for EvictionConfig {
    fn default() -> Self {
        Self { policy: EvictionPolicy::Lru, low_mark: 0.80, high_mark: 0.90 }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EvictionPolicy { Lru, Lfu, SizeDesc, Never }

// ── Pure parsing helpers (no I/O, no toml) ───────────────────────────────────

pub fn parse_size(s: &str) -> anyhow::Result<u64> {
    let s = s.trim();
    let split = s.find(|c: char| c.is_alphabetic()).unwrap_or(s.len());
    let (num_s, unit) = s.split_at(split);
    let num: f64 = num_s.trim().parse()
        .map_err(|_| anyhow::anyhow!("bad number in size {:?}", s))?;
    let mul = match unit.trim().to_ascii_uppercase().as_str() {
        "" | "B"         => 1u64,
        "K" | "KB" | "KIB" => 1 << 10,
        "M" | "MB" | "MIB" => 1 << 20,
        "G" | "GB" | "GIB" => 1 << 30,
        "T" | "TB" | "TIB" => 1u64 << 40,
        u => anyhow::bail!("unknown size unit {:?}", u),
    };
    Ok((num * mul as f64) as u64)
}

pub fn parse_duration(s: &str) -> anyhow::Result<Duration> {
    let s = s.trim();
    if let Some(n) = s.strip_suffix("ms") { return Ok(Duration::from_millis(n.trim().parse()?)); }
    if let Some(n) = s.strip_suffix('s')  { return Ok(Duration::from_secs(n.trim().parse()?)); }
    if let Some(n) = s.strip_suffix('m')  { return Ok(Duration::from_secs(n.trim().parse::<u64>()? * 60)); }
    if let Some(n) = s.strip_suffix('h')  { return Ok(Duration::from_secs(n.trim().parse::<u64>()? * 3600)); }
    Ok(Duration::from_secs(s.parse()?))
}

// ── Upload window ─────────────────────────────────────────────────────────────

/// Bandwidth schedule: a daily local-time interval during which uploads
/// are permitted. Stored as minutes-since-midnight; if `start_min ==
/// end_min`, the window is "always open" (degenerate, but tolerated).
/// If `start_min > end_min` the window crosses midnight (e.g. 22:00–06:00).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UploadWindow {
    pub start_min: u32, // 0..1440
    pub end_min:   u32, // 0..1440
}

impl UploadWindow {
    /// True if the given minute-of-day falls within the window.
    /// The window is half-open: `[start, end)` for non-wrapping, or
    /// `[start, 1440) ∪ [0, end)` when wrapping past midnight.
    pub fn contains_minute(&self, m: u32) -> bool {
        if self.start_min == self.end_min {
            // Degenerate: treat as 24/7 open (less surprising than 24/7 closed).
            return true;
        }
        if self.start_min < self.end_min {
            self.start_min <= m && m < self.end_min
        } else {
            // wraps past midnight
            m >= self.start_min || m < self.end_min
        }
    }

    /// Seconds from `now_min` (with `now_sec` seconds-into-minute) until
    /// the window opens. Returns 0 if currently open.
    pub fn seconds_until_open(&self, now_min: u32, now_sec: u32) -> u64 {
        if self.contains_minute(now_min) {
            return 0;
        }
        // Minutes from now until start_min, mod 1440.
        let now_total = now_min as i64 * 60 + now_sec as i64;
        let start_total = self.start_min as i64 * 60;
        let mut diff = start_total - now_total;
        let day = 24 * 60 * 60;
        if diff <= 0 { diff += day; }
        diff as u64
    }
}

/// Parse an `"HH:MM-HH:MM"` time-of-day window. Times are local-time;
/// if the start is later than the end the window crosses midnight.
pub fn parse_upload_window(s: &str) -> anyhow::Result<UploadWindow> {
    let s = s.trim();
    let (start, end) = s.split_once('-')
        .ok_or_else(|| anyhow::anyhow!("upload window must be HH:MM-HH:MM, got {s:?}"))?;
    let start_min = parse_hhmm(start.trim())
        .map_err(|e| anyhow::anyhow!("upload window start: {e}"))?;
    let end_min   = parse_hhmm(end.trim())
        .map_err(|e| anyhow::anyhow!("upload window end: {e}"))?;
    Ok(UploadWindow { start_min, end_min })
}

fn parse_hhmm(s: &str) -> anyhow::Result<u32> {
    let (h, m) = s.split_once(':')
        .ok_or_else(|| anyhow::anyhow!("expected HH:MM, got {s:?}"))?;
    let h: u32 = h.parse().map_err(|_| anyhow::anyhow!("bad hour in {s:?}"))?;
    let m: u32 = m.parse().map_err(|_| anyhow::anyhow!("bad minute in {s:?}"))?;
    if h >= 24 { anyhow::bail!("hour out of range: {h}"); }
    if m >= 60 { anyhow::bail!("minute out of range: {m}"); }
    Ok(h * 60 + m)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sizes() {
        assert_eq!(parse_size("5 GiB").unwrap(), 5 * (1 << 30));
        assert_eq!(parse_size("10GB").unwrap(),  10 * (1 << 30));
        assert_eq!(parse_size("500MB").unwrap(), 500 * (1 << 20));
        assert_eq!(parse_size("1024").unwrap(),  1024);
    }

    #[test]
    fn parse_durations() {
        assert_eq!(parse_duration("30s").unwrap(),   Duration::from_secs(30));
        assert_eq!(parse_duration("2m").unwrap(),    Duration::from_secs(120));
        assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
    }

    #[test]
    fn upload_window_basic_parse() {
        let w = parse_upload_window("22:00-06:00").unwrap();
        assert_eq!(w.start_min, 22 * 60);
        assert_eq!(w.end_min,    6 * 60);
    }

    #[test]
    fn upload_window_invalid_inputs() {
        assert!(parse_upload_window("").is_err());
        assert!(parse_upload_window("22:00").is_err());
        assert!(parse_upload_window("25:00-06:00").is_err());
        assert!(parse_upload_window("22:60-06:00").is_err());
        assert!(parse_upload_window("foo-bar").is_err());
    }

    #[test]
    fn upload_window_non_wrapping_contains() {
        let w = parse_upload_window("09:00-17:00").unwrap();
        assert!(!w.contains_minute(8 * 60 + 59));
        assert!( w.contains_minute(9 * 60));
        assert!( w.contains_minute(13 * 60));
        assert!(!w.contains_minute(17 * 60), "end is exclusive");
        assert!(!w.contains_minute(20 * 60));
    }

    #[test]
    fn upload_window_wrapping_contains() {
        let w = parse_upload_window("22:00-06:00").unwrap();
        assert!(!w.contains_minute(8 * 60));
        assert!(!w.contains_minute(20 * 60));
        assert!(!w.contains_minute(21 * 60 + 59));
        assert!( w.contains_minute(22 * 60));
        assert!( w.contains_minute(23 * 60));
        assert!( w.contains_minute(0));
        assert!( w.contains_minute(5 * 60 + 59));
        assert!(!w.contains_minute(6 * 60), "end is exclusive");
    }

    #[test]
    fn upload_window_seconds_until_open() {
        let w = parse_upload_window("22:00-06:00").unwrap();
        // Currently 21:00 → 1 hour until 22:00
        assert_eq!(w.seconds_until_open(21 * 60, 0), 3600);
        // Currently 23:00 → window is open, 0 seconds
        assert_eq!(w.seconds_until_open(23 * 60, 0), 0);
        // Currently 06:00 → window just closed, 16 hours until 22:00
        assert_eq!(w.seconds_until_open(6 * 60, 0), 16 * 3600);
        // Currently 21:59:30 → 30 seconds until 22:00
        assert_eq!(w.seconds_until_open(21 * 60 + 59, 30), 30);
    }

    #[test]
    fn upload_window_degenerate_equal_start_end_means_always_open() {
        let w = parse_upload_window("12:00-12:00").unwrap();
        assert!(w.contains_minute(0));
        assert!(w.contains_minute(12 * 60));
        assert!(w.contains_minute(23 * 60 + 59));
        assert_eq!(w.seconds_until_open(8 * 60, 0), 0);
    }
}
