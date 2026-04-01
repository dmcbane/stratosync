//! Config file loading — only place the `toml` crate is used.
use std::path::Path;
use anyhow::{Context, Result};
use stratosync_core::config::Config;

pub fn load(path: &Path) -> Result<Config> {
    let src = std::fs::read_to_string(path)
        .with_context(|| format!("read config {}", path.display()))?;
    let cfg: Config = toml::from_str(&src)
        .with_context(|| format!("parse config {}", path.display()))?;
    validate(&cfg)?;
    Ok(cfg)
}

pub fn validate(cfg: &Config) -> Result<()> {
    use std::collections::HashSet;
    let mut names = HashSet::new();
    let mut paths = HashSet::new();
    for m in &cfg.mounts {
        if m.name.is_empty()           { anyhow::bail!("mount name must not be empty"); }
        if !m.remote.contains(':')     { anyhow::bail!("mount.{}: remote must be rclone path (e.g. gdrive:/)", m.name); }
        m.cache_quota_bytes()?;
        let poll = m.poll_duration()?;
        if poll < std::time::Duration::from_secs(5) {
            anyhow::bail!("mount.{}: poll_interval must be >= 5s", m.name);
        }
        if !names.insert(m.name.clone())       { anyhow::bail!("duplicate mount name {:?}", m.name); }
        if !paths.insert(m.mount_path.clone()) { anyhow::bail!("duplicate mount_path {:?}", m.mount_path); }
    }
    Ok(())
}
