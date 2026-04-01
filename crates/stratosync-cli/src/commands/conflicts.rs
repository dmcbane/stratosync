use std::path::Path;
use anyhow::Result;
use stratosync_core::{state::StateDb};

pub async fn run(config_path: &Path) -> Result<()> {
    let cfg = crate::config_io::load(config_path)?;
    let db_path = stratosync_core::config::default_data_dir().join("state.db");

    if !db_path.exists() {
        println!("No state database found — daemon has not run yet.");
        return Ok(());
    }

    let db = StateDb::open(&db_path)?;
    let mut found_any = false;

    for mount in cfg.mounts.iter().filter(|m| m.enabled) {
        let Some(mount_id) = db.get_mount_id(&mount.name).await? else { continue };

        // Query for all conflict entries
        let conn = db.raw_conn().await;
        let mut stmt = conn.prepare(
            "SELECT inode, name, remote_path, size, mtime
             FROM file_index
             WHERE mount_id=?1 AND status='conflict'
             ORDER BY mtime DESC",
        )?;

        let rows: Vec<(u64, String, String, u64, i64)> = stmt
            .query_map(rusqlite::params![mount_id], |r| {
                Ok((
                    r.get::<_, i64>(0)? as u64,
                    r.get(1)?,
                    r.get(2)?,
                    r.get::<_, i64>(3)? as u64,
                    r.get(4)?,
                ))
            })?
            .filter_map(|r| r.ok())
            .collect();

        // Also look for files whose name contains ".conflict."
        let mut stmt2 = conn.prepare(
            "SELECT inode, name, remote_path, size, mtime
             FROM file_index
             WHERE mount_id=?1 AND name LIKE '%.conflict.%'
             ORDER BY mtime DESC",
        )?;
        let conflict_files: Vec<(u64, String, String, u64, i64)> = stmt2
            .query_map(rusqlite::params![mount_id], |r| {
                Ok((
                    r.get::<_, i64>(0)? as u64,
                    r.get(1)?,
                    r.get(2)?,
                    r.get::<_, i64>(3)? as u64,
                    r.get(4)?,
                ))
            })?
            .filter_map(|r| r.ok())
            .collect();

        if rows.is_empty() && conflict_files.is_empty() { continue; }

        found_any = true;
        println!("Mount: {}", mount.name);
        println!("{}", "─".repeat(60));

        for (inode, name, path, size, mtime) in &rows {
            let ts = chrono::DateTime::from_timestamp(*mtime, 0)
                .map(|d| d.format("%Y-%m-%d %H:%M UTC").to_string())
                .unwrap_or_else(|| "unknown".into());
            println!("  CONFLICT  {name}");
            println!("            inode={inode}  size={}  modified={ts}", bytesize::ByteSize(*size));
            println!("            remote: {path}");
            println!();
        }

        for (inode, name, path, size, mtime) in &conflict_files {
            let ts = chrono::DateTime::from_timestamp(*mtime, 0)
                .map(|d| d.format("%Y-%m-%d %H:%M UTC").to_string())
                .unwrap_or_else(|| "unknown".into());
            println!("  FILE      {name}");
            println!("            inode={inode}  size={}  modified={ts}", bytesize::ByteSize(*size));
            println!("            remote: {path}");
            println!();
        }
    }

    if !found_any {
        println!("No conflicts found.");
    } else {
        println!("To resolve:");
        println!("  stratosync conflicts keep-local  <path>   — upload local, discard remote");
        println!("  stratosync conflicts keep-remote <path>   — download remote, discard local");
    }

    Ok(())
}
