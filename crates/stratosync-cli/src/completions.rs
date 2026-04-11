/// Dynamic shell completions for stratosync CLI.
///
/// The conflict path completer queries the state DB at tab-completion time
/// to offer real conflict file paths — essential since conflict filenames
/// include timestamps and hashes that are impossible to type by hand.
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use clap_complete::CompletionCandidate;
use stratosync_core::config::default_data_dir;

/// Complete conflict file paths by querying each mount's state DB.
///
/// Used as an `ArgValueCompleter` on the `path` argument of
/// `conflicts keep-local/keep-remote/merge/diff`.
pub fn complete_conflict_path(current: &OsStr) -> Vec<CompletionCandidate> {
    complete_conflict_path_inner(current).unwrap_or_default()
}

fn complete_conflict_path_inner(current: &OsStr) -> Option<Vec<CompletionCandidate>> {
    let current_str = current.to_str().unwrap_or("");
    let config_path = stratosync_core::config::default_config_path();
    let cfg = crate::config_io::load(&config_path).ok()?;

    let mut candidates = Vec::new();

    for mount in cfg.mounts.iter().filter(|m| m.enabled) {
        let db_path = default_data_dir().join(format!("{}.db", mount.name));
        if !db_path.exists() { continue; }

        // Open a direct read-only connection (sync, no tokio needed).
        let conn = rusqlite::Connection::open_with_flags(
            &db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        ).ok();
        let Some(conn) = conn else { continue };

        let mount_id: Option<u32> = conn.query_row(
            "SELECT id FROM mounts WHERE name = ?1",
            rusqlite::params![mount.name],
            |r| r.get(0),
        ).ok();
        let Some(mount_id) = mount_id else { continue };

        let mount_abs = expand_tilde(&mount.resolved_mount_path());

        // Query entries with status='conflict' and `.conflict.` sibling files.
        collect_conflict_candidates(
            &conn, mount_id, &mount_abs, current_str, &mut candidates,
        );
    }

    Some(candidates)
}

fn collect_conflict_candidates(
    conn: &rusqlite::Connection,
    mount_id: u32,
    mount_path: &Path,
    current: &str,
    candidates: &mut Vec<CompletionCandidate>,
) {
    // Both queries: status='conflict' entries AND .conflict. sibling files.
    let sql = "SELECT remote_path, name, size FROM file_index \
               WHERE mount_id = ?1 AND (status = 'conflict' OR name LIKE '%.conflict.%') \
               ORDER BY mtime DESC";

    let Ok(mut stmt) = conn.prepare(sql) else { return };
    let rows = stmt.query_map(rusqlite::params![mount_id], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
        ))
    });
    let Ok(rows) = rows else { return };

    for row in rows.flatten() {
        let (remote_path, name, size) = row;

        // Build the local filesystem path: mount_path + remote_path
        let local_path = mount_path.join(remote_path.trim_start_matches('/'));
        let path_str = local_path.to_string_lossy();

        if path_str.starts_with(current) || current.is_empty() {
            let help = format!("{name} ({size} B)");
            candidates.push(
                CompletionCandidate::new(path_str.into_owned())
                    .help(Some(help.into()))
            );
        }
    }
}

fn expand_tilde(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    p.to_owned()
}
