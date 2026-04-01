// config.rs
use std::path::Path;
use anyhow::Result;
use stratosync_core::{backend::RcloneBackend, Backend};

pub fn show(config_path: &Path) -> Result<()> {
    let cfg = crate::config_io::load(config_path)?;
    println!("Config: {}", config_path.display());
    println!();
    for m in &cfg.mounts {
        println!(
            "  [{name}]  {status}",
            name = m.name,
            status = if m.enabled { "enabled" } else { "disabled" },
        );
        println!("    remote:       {}", m.remote);
        println!("    mount_path:   {}", m.mount_path.display());
        println!("    cache_quota:  {}", m.cache_quota);
        println!("    poll_interval:{}", m.poll_interval);
        println!();
    }
    Ok(())
}

pub async fn test(config_path: &Path) -> Result<()> {
    let cfg = crate::config_io::load(config_path)?;
    println!("Testing rclone connectivity...\n");

    for m in cfg.mounts.iter().filter(|m| m.enabled) {
        print!("  {}: ", m.name);
        let backend = RcloneBackend::new(&m.remote)?;
        match backend.about().await {
            Ok(info) => {
                let used  = info.used.map(|u| bytesize::ByteSize(u).to_string()).unwrap_or("?".into());
                let total = info.total.map(|t| bytesize::ByteSize(t).to_string()).unwrap_or("?".into());
                println!("✓  {used} used of {total}");
            }
            Err(e) => println!("✗  {e}"),
        }
    }
    Ok(())
}

pub fn edit(config_path: &Path) -> Result<()> {
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "nano".into());
    std::process::Command::new(&editor)
        .arg(config_path)
        .status()?;
    Ok(())
}
