use std::path::Path;
use anyhow::Result;
use bytesize::ByteSize;
use stratosync_core::{config::default_data_dir, state::StateDb};

pub async fn run(config_path: &Path) -> Result<()> {
    let cfg = crate::config_io::load(config_path)?;

    for mount in &cfg.mounts {
        let db_path = default_data_dir().join(format!("{}.db", mount.name));

        if !db_path.exists() {
            println!("  {} — not yet mounted", mount.name);
            continue;
        }

        let db = StateDb::open(&db_path)?;
        let Some(mount_id) = db.get_mount_id(&mount.name).await? else {
            println!("  {} — not yet mounted", mount.name);
            continue;
        };

        let total_cache = db.total_cache_bytes(mount_id).await?;
        let quota       = mount.cache_quota_bytes()?;
        let pct         = (total_cache as f64 / quota as f64 * 100.0) as u64;

        println!(
            "  {}  {}  cache: {}/{} ({}%)",
            if mount.enabled { "●" } else { "○" },
            mount.name,
            ByteSize(total_cache),
            ByteSize(quota),
            pct,
        );

        println!("    mount: {}", mount.mount_path.display());
        println!("    remote: {}", mount.remote);
    }

    Ok(())
}
