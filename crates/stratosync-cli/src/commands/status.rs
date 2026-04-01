use std::path::Path;
use anyhow::Result;
use bytesize::ByteSize;
use stratosync_core::{state::StateDb};

pub async fn run(config_path: &Path) -> Result<()> {
    let cfg = crate::config_io::load(config_path)?;
    let db_path = stratosync_core::config::default_data_dir().join("state.db");

    if !db_path.exists() {
        println!("No state database found — daemon has not run yet.");
        println!("Start with:  stratosyncd");
        return Ok(());
    }

    let db = StateDb::open(&db_path)?;

    for mount in &cfg.mounts {
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

        // Count files by status
        // (In a full implementation, we'd have a summary query in StateDb)
        println!("    mount: {}", mount.mount_path.display());
        println!("    remote: {}", mount.remote);
    }

    Ok(())
}
