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
    pub socket:    Option<PathBuf>,
    pub fuse:      FuseConfig,
    pub sync:      SyncConfig,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            log_level: LogLevel::Info,
            log_file:  None,
            pid_file:  None,
            socket:    None,
            fuse:      FuseConfig::default(),
            sync:      SyncConfig::default(),
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
}

fn default_cache_quota_str() -> String { "5 GiB".into()  }
fn default_poll_interval()   -> String { "60s".into()    }
fn yes() -> bool { true }

impl MountConfig {
    pub fn cache_quota_bytes(&self) -> anyhow::Result<u64>    { parse_size(&self.cache_quota)        }
    pub fn poll_duration(&self)     -> anyhow::Result<Duration> { parse_duration(&self.poll_interval) }
    pub fn resolved_mount_path(&self) -> PathBuf { expand_tilde(&self.mount_path) }
    pub fn cache_dir(&self)         -> PathBuf   { default_cache_dir().join(&self.name) }
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
}
