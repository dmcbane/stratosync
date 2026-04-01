//! Config file loading for the daemon (uses toml crate).
use std::path::Path;
use anyhow::{Context, Result};
use stratosync_core::config::Config;

pub fn load(path: &Path) -> Result<Config> {
    let src = std::fs::read_to_string(path)
        .with_context(|| format!("read config {}", path.display()))?;
    toml::from_str(&src)
        .with_context(|| format!("parse config {}", path.display()))
}
