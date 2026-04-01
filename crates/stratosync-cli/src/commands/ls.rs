// ls.rs — list directory via backend
use std::path::Path;
use anyhow::Result;
use bytesize::ByteSize;
use stratosync_core::{backend::RcloneBackend, Backend};

pub async fn run(config_path: &Path, path: Option<&Path>, _all: bool) -> Result<()> {
    let cfg = crate::config_io::load(config_path)?;

    // Determine which mount and relative path
    let (mount, rel_path) = if let Some(p) = path {
        let p = expand_tilde(p);
        cfg.mounts.iter()
            .filter_map(|m| {
                let mount_abs = expand_tilde(&m.resolved_mount_path());
                p.strip_prefix(&mount_abs).ok().map(|rel| (m, rel.to_path_buf()))
            })
            .next()
            .ok_or_else(|| anyhow::anyhow!("path {} is not under any configured mount", p.display()))?
    } else {
        // Default: list root of first mount
        let m = cfg.mounts.first()
            .ok_or_else(|| anyhow::anyhow!("no mounts configured"))?;
        (m, std::path::PathBuf::new())
    };

    let backend = RcloneBackend::new(&mount.remote)?;
    let rel_str = rel_path.to_string_lossy();
    let entries = backend.list(&rel_str).await?;

    for e in &entries {
        let size_str = if e.is_dir {
            "         -".to_owned()
        } else {
            format!("{:>10}", ByteSize(e.size))
        };
        let mtime = chrono::DateTime::<chrono::Utc>::from(e.mtime)
            .format("%Y-%m-%d %H:%M")
            .to_string();
        let name = if e.is_dir { format!("{}/", e.name) } else { e.name.clone() };
        println!("{size_str}  {mtime}  {name}");
    }

    if entries.is_empty() {
        println!("(empty)");
    }

    Ok(())
}

fn expand_tilde(p: &Path) -> std::path::PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    p.to_owned()
}
