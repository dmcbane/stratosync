/// SQLite state database — the daemon's persistent store.
///
/// See docs/architecture/04-state-db.md for full schema documentation.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use tokio::sync::Mutex;
use tracing::{debug, info};

use crate::types::{FileEntry, FileKind, Inode, SyncStatus, FUSE_ROOT_INODE};

/// Lightweight snapshot of a file_index row for in-memory diffing.
#[derive(Debug, Clone)]
pub struct RemoteSnapshot {
    pub inode:  Inode,
    pub etag:   Option<String>,
    pub size:   u64,
    pub status: SyncStatus,
}

// ── DB wrapper ────────────────────────────────────────────────────────────────

/// Thread-safe wrapper around a SQLite connection.
///
/// We use a single writer connection (WAL mode) shared behind a Mutex.
/// Read-heavy paths should use a separate read connection pool (Phase 2).
#[derive(Clone)]
pub struct StateDb {
    conn: Arc<Mutex<Connection>>,
}

impl StateDb {
    /// Open (or create) the database at the given path.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create state dir {parent:?}"))?;
        }

        let conn = Connection::open(path)
            .with_context(|| format!("open state DB at {path:?}"))?;

        // Pragmas — must run before any schema work
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous   = NORMAL;
             PRAGMA foreign_keys  = ON;
             PRAGMA busy_timeout  = 5000;
             PRAGMA auto_vacuum   = INCREMENTAL;",
        )?;

        let db = Self { conn: Arc::new(Mutex::new(conn)) };
        Ok(db)
    }

    /// Open an in-memory database (for tests).
    pub fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA foreign_keys  = ON;",
        )?;
        Ok(Self { conn: Arc::new(Mutex::new(conn)) })
    }

    // ── Migration ────────────────────────────────────────────────────────────

    pub async fn migrate(&self) -> Result<()> {
        let conn = self.conn.lock().await;
        run_migrations(&conn)
    }

    // ── Mount CRUD ───────────────────────────────────────────────────────────

    pub async fn upsert_mount(
        &self,
        name:        &str,
        remote:      &str,
        mount_path:  &str,
        cache_dir:   &str,
        cache_quota: u64,
        poll_secs:   u32,
    ) -> Result<u32> {
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO mounts (name, remote, mount_path, cache_dir, cache_quota, poll_secs)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(name) DO UPDATE SET
               remote=excluded.remote,
               mount_path=excluded.mount_path,
               cache_dir=excluded.cache_dir,
               cache_quota=excluded.cache_quota,
               poll_secs=excluded.poll_secs",
            params![name, remote, mount_path, cache_dir, cache_quota as i64, poll_secs],
        )?;
        let id: u32 = conn.query_row(
            "SELECT id FROM mounts WHERE name = ?1",
            params![name],
            |r| r.get(0),
        )?;
        Ok(id)
    }

    pub async fn get_mount_id(&self, name: &str) -> Result<Option<u32>> {
        let conn = self.conn.lock().await;
        let id = conn.query_row(
            "SELECT id FROM mounts WHERE name = ?1",
            params![name],
            |r| r.get(0),
        ).optional()?;
        Ok(id)
    }

    // ── File index: insert / update ──────────────────────────────────────────

    /// Insert the FUSE root directory at inode 1 with NULL parent.
    /// Uses an explicit inode value to avoid self-referencing FK issues
    /// (autoincrement may not start at 1 if the table was previously populated).
    pub async fn insert_root(&self, entry: &NewFileEntry) -> Result<Inode> {
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT OR REPLACE INTO file_index
               (inode, mount_id, parent_inode, name, remote_path, kind, size, mtime,
                etag, status, cache_path, cache_size)
             VALUES (?1, ?2, NULL, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                FUSE_ROOT_INODE as i64,
                entry.mount_id,
                entry.name,
                entry.remote_path,
                entry.kind.as_str(),
                entry.size as i64,
                to_unix(entry.mtime),
                entry.etag,
                entry.status.as_str(),
                entry.cache_path.as_ref().map(|p| p.to_str()),
                entry.cache_size.map(|s| s as i64),
            ],
        )?;
        Ok(FUSE_ROOT_INODE)
    }

    /// Insert a new file entry. Returns the new inode.
    pub async fn insert_file(&self, entry: &NewFileEntry) -> Result<Inode> {
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO file_index
               (mount_id, parent_inode, name, remote_path, kind, size, mtime,
                etag, status, cache_path, cache_size)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                entry.mount_id,
                entry.parent,
                entry.name,
                entry.remote_path,
                entry.kind.as_str(),
                entry.size as i64,
                to_unix(entry.mtime),
                entry.etag,
                entry.status.as_str(),
                entry.cache_path.as_ref().map(|p| p.to_str()),
                entry.cache_size.map(|s| s as i64),
            ],
        )?;
        Ok(conn.last_insert_rowid() as u64)
    }

    /// Batch upsert children of a directory in a single transaction.
    /// Much faster than individual upserts (single mutex acquire, single txn).
    pub async fn batch_upsert_remote_files(
        &self,
        mount_id:   u32,
        parent:     Inode,
        entries:    &[(String, String, FileKind, u64, SystemTime, Option<String>)],
    ) -> Result<()> {
        self.batch_upsert_remote_files_gen(mount_id, parent, entries, 0).await
    }

    /// Batch upsert with explicit poll_generation.
    pub async fn batch_upsert_remote_files_gen(
        &self,
        mount_id:       u32,
        parent:         Inode,
        entries:        &[(String, String, FileKind, u64, SystemTime, Option<String>)],
        poll_generation: u64,
    ) -> Result<()> {
        // Process in chunks to avoid holding the mutex for too long
        const CHUNK_SIZE: usize = 1000;
        for chunk in entries.chunks(CHUNK_SIZE) {
            let conn = self.conn.lock().await;
            conn.execute_batch("BEGIN")?;
            let result: Result<()> = (|| {
                for (name, remote_path, kind, size, mtime, etag) in chunk {
                    conn.execute(
                        "INSERT INTO file_index
                           (mount_id, parent_inode, name, remote_path, kind, size, mtime, etag, status, poll_generation)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'remote', ?9)
                         ON CONFLICT(mount_id, remote_path) DO UPDATE SET
                           parent_inode    = excluded.parent_inode,
                           name            = excluded.name,
                           kind            = excluded.kind,
                           size            = excluded.size,
                           mtime           = excluded.mtime,
                           etag            = excluded.etag,
                           poll_generation = excluded.poll_generation,
                           status          = CASE
                             WHEN status IN ('dirty','uploading') THEN status
                             ELSE 'stale'
                           END",
                        params![
                            mount_id, parent as i64, name, remote_path,
                            kind.as_str(), *size as i64, to_unix(*mtime), etag.as_deref(),
                            poll_generation as i64,
                        ],
                    )?;
                }
                Ok(())
            })();
            match &result {
                Ok(()) => conn.execute_batch("COMMIT")?,
                Err(_) => { let _ = conn.execute_batch("ROLLBACK"); }
            }
            result?;
        }
        Ok(())
    }

    /// Upsert by (mount_id, remote_path) — used during remote listing.
    pub async fn upsert_remote_file(
        &self,
        mount_id:    u32,
        parent:      Inode,
        name:        &str,
        remote_path: &str,
        kind:        FileKind,
        size:        u64,
        mtime:       SystemTime,
        etag:        Option<&str>,
    ) -> Result<Inode> {
        self.upsert_remote_file_gen(mount_id, parent, name, remote_path, kind, size, mtime, etag, 0).await
    }

    /// Upsert with explicit poll_generation.
    pub async fn upsert_remote_file_gen(
        &self,
        mount_id:        u32,
        parent:          Inode,
        name:            &str,
        remote_path:     &str,
        kind:            FileKind,
        size:            u64,
        mtime:           SystemTime,
        etag:            Option<&str>,
        poll_generation: u64,
    ) -> Result<Inode> {
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO file_index
               (mount_id, parent_inode, name, remote_path, kind, size, mtime, etag, status, poll_generation)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'remote', ?9)
             ON CONFLICT(mount_id, remote_path) DO UPDATE SET
               parent_inode    = excluded.parent_inode,
               name            = excluded.name,
               kind            = excluded.kind,
               size            = excluded.size,
               mtime           = excluded.mtime,
               etag            = excluded.etag,
               poll_generation = excluded.poll_generation,
               status          = CASE
                 WHEN status IN ('dirty','uploading') THEN status
                 ELSE 'stale'
               END",
            params![
                mount_id, parent, name, remote_path,
                kind.as_str(), size as i64, to_unix(mtime), etag,
                poll_generation as i64,
            ],
        )?;

        let inode: Inode = conn.query_row(
            "SELECT inode FROM file_index WHERE mount_id=?1 AND remote_path=?2",
            params![mount_id, remote_path],
            |r| r.get::<_, i64>(0),
        ).map(|i| i as u64)?;

        Ok(inode)
    }

    // ── File index: reads ─────────────────────────────────────────────────────

    pub async fn get_by_inode(&self, inode: Inode) -> Result<Option<FileEntry>> {
        let conn = self.conn.lock().await;
        let row = conn.query_row(
            "SELECT inode, mount_id, parent_inode, name, remote_path, kind,
                    size, mtime, etag, status, cache_path, cache_size, dir_listed
             FROM file_index WHERE inode = ?1",
            params![inode as i64],
            row_to_entry,
        ).optional()?;
        Ok(row)
    }

    pub async fn get_by_parent_name(
        &self,
        mount_id: u32,
        parent:   Inode,
        name:     &str,
    ) -> Result<Option<FileEntry>> {
        let conn = self.conn.lock().await;
        conn.query_row(
            "SELECT inode, mount_id, parent_inode, name, remote_path, kind,
                    size, mtime, etag, status, cache_path, cache_size, dir_listed
             FROM file_index WHERE mount_id = ?1 AND parent_inode = ?2 AND name = ?3",
            params![mount_id, parent as i64, name],
            row_to_entry,
        ).optional().map_err(Into::into)
    }

    pub async fn list_children(&self, mount_id: u32, parent: Inode) -> Result<Vec<FileEntry>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT inode, mount_id, parent_inode, name, remote_path, kind,
                    size, mtime, etag, status, cache_path, cache_size, dir_listed
             FROM file_index WHERE mount_id = ?1 AND parent_inode = ?2
             ORDER BY inode ASC",
        )?;
        let rows = stmt.query_map(params![mount_id, parent as i64], row_to_entry)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // ── Status transitions ────────────────────────────────────────────────────

    pub async fn set_status(&self, inode: Inode, status: SyncStatus) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "UPDATE file_index SET status = ?1 WHERE inode = ?2",
            params![status.as_str(), inode as i64],
        )?;
        Ok(())
    }

    /// Mark a file as dirty and update its size (for getattr accuracy after writes).
    pub async fn set_dirty_size(&self, inode: Inode, size: u64) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "UPDATE file_index SET status='dirty', size=?1, cache_size=?1 WHERE inode=?2",
            params![size as i64, inode as i64],
        )?;
        Ok(())
    }

    pub async fn set_cached(
        &self,
        inode:      Inode,
        cache_path: &Path,
        cache_size: u64,
        etag:       Option<&str>,
        mtime:      SystemTime,
        size:       u64,
    ) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "UPDATE file_index
             SET status='cached', cache_path=?1, cache_size=?2,
                 etag=?3, mtime=?4, size=?5, cache_mtime=unixepoch()
             WHERE inode = ?6",
            params![
                cache_path.to_str(),
                cache_size as i64,
                etag,
                to_unix(mtime),
                size as i64,
                inode as i64,
            ],
        )?;
        // Touch LRU
        conn.execute(
            "INSERT INTO cache_lru (inode, last_access) VALUES (?1, unixepoch())
             ON CONFLICT(inode) DO UPDATE SET last_access=unixepoch()",
            params![inode as i64],
        )?;
        Ok(())
    }

    pub async fn set_evicted(&self, inode: Inode) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "UPDATE file_index
             SET status='remote', cache_path=NULL, cache_size=NULL, cache_mtime=NULL
             WHERE inode = ?1",
            params![inode as i64],
        )?;
        conn.execute(
            "DELETE FROM cache_lru WHERE inode = ?1",
            params![inode as i64],
        )?;
        Ok(())
    }

    pub async fn touch_lru(&self, inode: Inode) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO cache_lru (inode, last_access) VALUES (?1, unixepoch())
             ON CONFLICT(inode) DO UPDATE SET last_access=unixepoch()",
            params![inode as i64],
        )?;
        Ok(())
    }

    pub async fn mark_dir_listed(&self, inode: Inode) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "UPDATE file_index SET dir_listed = unixepoch() WHERE inode = ?1",
            params![inode as i64],
        )?;
        Ok(())
    }

    // ── Sync queue ────────────────────────────────────────────────────────────

    pub async fn enqueue_upload(
        &self,
        inode:       Inode,
        mount_id:    u32,
        remote_path: &str,
        known_etag:  Option<&str>,
        priority:    i32,
    ) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO sync_queue (inode, mount_id, op, remote_path, known_etag, priority)
             VALUES (?1, ?2, 'upload', ?3, ?4, ?5)
             ON CONFLICT DO NOTHING",
            params![inode as i64, mount_id, remote_path, known_etag, priority],
        )?;
        Ok(())
    }

    pub async fn dequeue_next_upload(&self, mount_id: u32) -> Result<Option<SyncQueueJob>> {
        let conn = self.conn.lock().await;
        let job = conn.query_row(
            "SELECT q.id, q.inode, q.op, q.remote_path, q.remote_dest,
                    q.known_etag, q.priority, q.attempt, f.cache_path
             FROM sync_queue q
             JOIN file_index f ON f.inode = q.inode
             WHERE q.mount_id = ?1
               AND q.op = 'upload'
               AND q.next_attempt <= unixepoch()
             ORDER BY q.priority DESC, q.created_at ASC
             LIMIT 1",
            params![mount_id],
            |r| Ok(SyncQueueJob {
                id:          r.get(0)?,
                inode:       r.get::<_, i64>(1)? as u64,
                op:          r.get(2)?,
                remote_path: r.get(3)?,
                remote_dest: r.get(4)?,
                known_etag:  r.get(5)?,
                priority:    r.get(6)?,
                attempt:     r.get(7)?,
                cache_path:  r.get::<_, Option<String>>(8)?
                               .map(PathBuf::from),
            }),
        ).optional()?;
        Ok(job)
    }

    pub async fn complete_queue_job(&self, job_id: i64) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute("DELETE FROM sync_queue WHERE id = ?1", params![job_id])?;
        Ok(())
    }

    pub async fn fail_queue_job(
        &self,
        job_id:    i64,
        error_msg: &str,
        backoff_s: u64,
    ) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "UPDATE sync_queue
             SET attempt=attempt+1,
                 error_msg=?1,
                 next_attempt=unixepoch()+?2
             WHERE id=?3",
            params![error_msg, backoff_s as i64, job_id],
        )?;
        Ok(())
    }

    // ── Cache LRU ─────────────────────────────────────────────────────────────

    pub async fn total_cache_bytes(&self, mount_id: u32) -> Result<u64> {
        let conn = self.conn.lock().await;
        let total: i64 = conn.query_row(
            "SELECT COALESCE(SUM(cache_size), 0) FROM file_index
             WHERE mount_id=?1 AND cache_size IS NOT NULL",
            params![mount_id],
            |r| r.get(0),
        )?;
        Ok(total as u64)
    }

    pub async fn lru_eviction_candidates(
        &self,
        mount_id: u32,
        limit:    usize,
    ) -> Result<Vec<EvictionCandidate>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT f.inode, f.cache_path, f.cache_size
             FROM file_index f
             JOIN cache_lru l ON l.inode = f.inode
             WHERE f.mount_id = ?1
               AND f.status = 'cached'
               AND l.pinned = 0
             ORDER BY l.last_access ASC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![mount_id, limit as i64], |r| {
            Ok(EvictionCandidate {
                inode:      r.get::<_, i64>(0)? as u64,
                cache_path: r.get::<_, String>(1).map(PathBuf::from)?,
                cache_size: r.get::<_, i64>(2)? as u64,
            })
        })?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // ── Pinning ───────────────────────────────────────────────────────────────

    /// Pin a file — prevents LRU eviction.
    pub async fn set_pinned(&self, inode: Inode, pinned: bool) -> Result<()> {
        let conn = self.conn.lock().await;
        let val = if pinned { 1i64 } else { 0 };
        conn.execute(
            "INSERT INTO cache_lru (inode, last_access, pinned) VALUES (?1, unixepoch(), ?2)
             ON CONFLICT(inode) DO UPDATE SET pinned = ?2",
            params![inode as i64, val],
        )?;
        Ok(())
    }

    /// Check if a file is pinned.
    pub async fn is_pinned(&self, inode: Inode) -> Result<bool> {
        let conn = self.conn.lock().await;
        let pinned: i64 = conn.query_row(
            "SELECT COALESCE(pinned, 0) FROM cache_lru WHERE inode = ?1",
            params![inode as i64],
            |r| r.get(0),
        ).unwrap_or(0);
        Ok(pinned != 0)
    }

    /// Count pinned files for a mount.
    pub async fn pinned_count(&self, mount_id: u32) -> Result<u64> {
        let conn = self.conn.lock().await;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM cache_lru l
             JOIN file_index f ON l.inode = f.inode
             WHERE f.mount_id = ?1 AND l.pinned = 1",
            params![mount_id],
            |r| r.get(0),
        )?;
        Ok(count as u64)
    }

    /// List all file descendants under a remote path prefix.
    pub async fn list_file_descendants(
        &self, mount_id: u32, remote_path_prefix: &str,
    ) -> Result<Vec<FileEntry>> {
        let conn = self.conn.lock().await;
        let pattern = format!("{}%", remote_path_prefix.trim_end_matches('/').to_owned() + "/");
        let mut stmt = conn.prepare(
            "SELECT inode, mount_id, parent_inode, name, remote_path, kind,
                    size, mtime, etag, status, cache_path, cache_size, dir_listed
             FROM file_index
             WHERE mount_id = ?1 AND remote_path LIKE ?2 AND kind = 'file'
             ORDER BY remote_path ASC",
        )?;
        let rows = stmt.query_map(params![mount_id, pattern], row_to_entry)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // ── Recovery on restart ───────────────────────────────────────────────────

    /// Reset all HYDRATING entries back to REMOTE (crash recovery).
    pub async fn reset_hydrating(&self) -> Result<usize> {
        let conn = self.conn.lock().await;
        let n = conn.execute(
            "UPDATE file_index SET status='remote', cache_path=NULL, cache_size=NULL
             WHERE status='hydrating'",
            [],
        )?;
        if n > 0 { info!(count = n, "reset hydrating entries after restart"); }
        Ok(n)
    }

    /// Reset all UPLOADING entries back to DIRTY (upload loop crashed).
    pub async fn reset_uploading(&self) -> Result<usize> {
        let conn = self.conn.lock().await;
        let n = conn.execute(
            "UPDATE file_index SET status='dirty' WHERE status='uploading'",
            [],
        )?;
        if n > 0 { info!(count = n, "reset uploading entries to dirty"); }
        Ok(n)
    }

    /// Return all entries that were UPLOADING (need retry after restart).
    pub async fn get_pending_uploads(&self, mount_id: u32) -> Result<Vec<FileEntry>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT inode, mount_id, parent_inode, name, remote_path, kind,
                    size, mtime, etag, status, cache_path, cache_size, dir_listed
             FROM file_index WHERE mount_id=?1 AND status IN ('dirty','uploading')",
        )?;
        let rows = stmt.query_map(params![mount_id], row_to_entry)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}

// ── Data structures ───────────────────────────────────────────────────────────

pub struct NewFileEntry {
    pub mount_id:    u32,
    pub parent:      Inode,
    pub name:        String,
    pub remote_path: String,
    pub kind:        FileKind,
    pub size:        u64,
    pub mtime:       SystemTime,
    pub etag:        Option<String>,
    pub status:      SyncStatus,
    pub cache_path:  Option<PathBuf>,
    pub cache_size:  Option<u64>,
}

#[derive(Debug)]
pub struct SyncQueueJob {
    pub id:          i64,
    pub inode:       Inode,
    pub op:          String,
    pub remote_path: String,
    pub remote_dest: Option<String>,
    pub known_etag:  Option<String>,
    pub priority:    i32,
    pub attempt:     u32,
    pub cache_path:  Option<PathBuf>,
}

impl SyncQueueJob {
    pub fn backoff_secs(&self) -> u64 {
        // Exponential backoff: 5s, 10s, 20s, 40s, ... capped at 600s
        let base: u64 = 5;
        (base * 2u64.pow(self.attempt.min(7))).min(600)
    }
}

#[derive(Debug)]
pub struct EvictionCandidate {
    pub inode:      Inode,
    pub cache_path: PathBuf,
    pub cache_size: u64,
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn to_unix(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO).as_secs() as i64
}

fn from_unix(secs: i64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(secs.max(0) as u64)
}

fn row_to_entry(r: &rusqlite::Row<'_>) -> rusqlite::Result<FileEntry> {
    let kind_str: String = r.get(5)?;
    let kind = match kind_str.as_str() {
        "dir"     => FileKind::Directory,
        "symlink" => FileKind::Symlink,
        _         => FileKind::File,
    };

    let status_str: String = r.get(9)?;
    let status = SyncStatus::from_str(&status_str).unwrap_or(SyncStatus::Remote);

    Ok(FileEntry {
        inode:      r.get::<_, i64>(0)? as u64,
        mount_id:   r.get::<_, i32>(1)? as u32,
        parent:     r.get::<_, Option<i64>>(2)?.unwrap_or(0) as u64,
        name:       r.get(3)?,
        remote_path: r.get(4)?,
        kind,
        size:       r.get::<_, i64>(6)? as u64,
        mtime:      from_unix(r.get(7)?),
        etag:       r.get(8)?,
        status,
        cache_path: r.get::<_, Option<String>>(10)?.map(PathBuf::from),
        cache_size: r.get::<_, Option<i64>>(11)?.map(|s| s as u64),
        dir_listed: r.get::<_, Option<i64>>(12)?.map(from_unix),
    })
}

impl FileKind {
    pub fn as_str(self) -> &'static str {
        match self {
            FileKind::File      => "file",
            FileKind::Directory => "dir",
            FileKind::Symlink   => "symlink",
        }
    }
}

// ── Migrations ────────────────────────────────────────────────────────────────

const MIGRATIONS: &[(&str, &str)] = &[
    ("0001", include_str!("migrations/0001_initial.sql")),
    ("0002", include_str!("migrations/0002_delete_tombstones.sql")),
    ("0003", include_str!("migrations/0003_poll_generation.sql")),
    ("0004", include_str!("migrations/0004_base_versions.sql")),
];

fn run_migrations(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            version     TEXT PRIMARY KEY,
            applied_at  INTEGER NOT NULL DEFAULT (unixepoch()),
            description TEXT
        )",
    )?;

    for (version, sql) in MIGRATIONS {
        let already_run: bool = conn.query_row(
            "SELECT COUNT(*) > 0 FROM schema_migrations WHERE version = ?1",
            params![version],
            |r| r.get(0),
        )?;

        if !already_run {
            debug!(version, "applying migration");
            conn.execute_batch(sql)?;
            conn.execute(
                "INSERT INTO schema_migrations (version) VALUES (?1)",
                params![version],
            )?;
            info!(version, "migration applied");
        }
    }

    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn migration_is_idempotent() {
        let db = StateDb::in_memory().unwrap();
        db.migrate().await.unwrap();
        db.migrate().await.unwrap();  // second run must not fail
    }

    #[tokio::test]
    async fn upsert_and_lookup_file() {
        let db = StateDb::in_memory().unwrap();
        db.migrate().await.unwrap();

        let mount_id = db.upsert_mount(
            "test", "local:/", "/mnt/test",
            "/tmp/cache", 5 * 1024 * 1024 * 1024, 60,
        ).await.unwrap();

        // Insert root directory
        let root_inode = db.insert_root(&NewFileEntry {
            mount_id,
            parent: 0, // ignored by insert_root
            name: "/".into(),
            remote_path: "/".into(),
            kind: FileKind::Directory,
            size: 0,
            mtime: SystemTime::now(),
            etag: None,
            status: SyncStatus::Remote,
            cache_path: None,
            cache_size: None,
        }).await.unwrap();

        let inode = db.insert_file(&NewFileEntry {
            mount_id,
            parent: root_inode,
            name:  "test.txt".into(),
            remote_path: "/test.txt".into(),
            kind:  FileKind::File,
            size:  42,
            mtime: SystemTime::now(),
            etag:  Some("abc123".into()),
            status: SyncStatus::Remote,
            cache_path: None,
            cache_size: None,
        }).await.unwrap();

        let entry = db.get_by_inode(inode).await.unwrap().unwrap();
        assert_eq!(entry.name, "test.txt");
        assert_eq!(entry.status, SyncStatus::Remote);
        assert_eq!(entry.size, 42);

        db.set_status(inode, SyncStatus::Dirty).await.unwrap();
        let entry = db.get_by_inode(inode).await.unwrap().unwrap();
        assert_eq!(entry.status, SyncStatus::Dirty);
    }

    // ── Change token tests ───────────────────────────────────────────────

    async fn setup_db_with_mount() -> (StateDb, u32, Inode) {
        let db = StateDb::in_memory().unwrap();
        db.migrate().await.unwrap();
        let mount_id = db.upsert_mount(
            "test", "gdrive:/", "/mnt/test",
            "/tmp/cache", 5 * 1024 * 1024 * 1024, 60,
        ).await.unwrap();
        let root = db.insert_root(&NewFileEntry {
            mount_id,
            parent: 0,
            name: "/".into(),
            remote_path: "/".into(),
            kind: FileKind::Directory,
            size: 0,
            mtime: SystemTime::now(),
            etag: None,
            status: SyncStatus::Remote,
            cache_path: None,
            cache_size: None,
        }).await.unwrap();
        (db, mount_id, root)
    }

    #[tokio::test]
    async fn change_token_round_trip() {
        let (db, mount_id, _) = setup_db_with_mount().await;

        // Initially no token
        assert!(db.get_change_token(mount_id).await.unwrap().is_none());

        // Store a token
        db.set_change_token(mount_id, "page-token-123").await.unwrap();
        assert_eq!(
            db.get_change_token(mount_id).await.unwrap().unwrap(),
            "page-token-123"
        );

        // Overwrite the token
        db.set_change_token(mount_id, "page-token-456").await.unwrap();
        assert_eq!(
            db.get_change_token(mount_id).await.unwrap().unwrap(),
            "page-token-456"
        );
    }

    #[tokio::test]
    async fn change_token_clear() {
        let (db, mount_id, _) = setup_db_with_mount().await;

        db.set_change_token(mount_id, "page-token-123").await.unwrap();
        db.clear_change_token(mount_id).await.unwrap();
        assert!(db.get_change_token(mount_id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn change_token_clear_nonexistent() {
        let (db, mount_id, _) = setup_db_with_mount().await;
        // Clearing a nonexistent token should not error
        db.clear_change_token(mount_id).await.unwrap();
    }

    #[tokio::test]
    async fn delete_remote_entry_by_path_existing() {
        let (db, mount_id, root) = setup_db_with_mount().await;

        let inode = db.insert_file(&NewFileEntry {
            mount_id,
            parent: root,
            name: "doc.txt".into(),
            remote_path: "doc.txt".into(),
            kind: FileKind::File,
            size: 100,
            mtime: SystemTime::now(),
            etag: Some("etag1".into()),
            status: SyncStatus::Remote,
            cache_path: None,
            cache_size: None,
        }).await.unwrap();

        let result = db.delete_remote_entry_by_path(mount_id, "doc.txt").await.unwrap();
        assert!(result.is_some());
        assert_eq!(result.unwrap().0, inode);

        // Entry should be gone
        assert!(db.get_by_inode(inode).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn delete_remote_entry_by_path_nonexistent() {
        let (db, mount_id, _) = setup_db_with_mount().await;
        let result = db.delete_remote_entry_by_path(mount_id, "no-such-file.txt").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn delete_remote_entry_by_path_protects_dirty() {
        let (db, mount_id, root) = setup_db_with_mount().await;

        let inode = db.insert_file(&NewFileEntry {
            mount_id,
            parent: root,
            name: "dirty.txt".into(),
            remote_path: "dirty.txt".into(),
            kind: FileKind::File,
            size: 100,
            mtime: SystemTime::now(),
            etag: Some("etag1".into()),
            status: SyncStatus::Dirty,
            cache_path: None,
            cache_size: None,
        }).await.unwrap();

        // Should NOT delete dirty entries
        let result = db.delete_remote_entry_by_path(mount_id, "dirty.txt").await.unwrap();
        assert!(result.is_none());

        // Entry should still exist
        assert!(db.get_by_inode(inode).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn delete_remote_entry_by_path_protects_uploading() {
        let (db, mount_id, root) = setup_db_with_mount().await;

        let inode = db.insert_file(&NewFileEntry {
            mount_id,
            parent: root,
            name: "uploading.txt".into(),
            remote_path: "uploading.txt".into(),
            kind: FileKind::File,
            size: 100,
            mtime: SystemTime::now(),
            etag: Some("etag1".into()),
            status: SyncStatus::Uploading,
            cache_path: None,
            cache_size: None,
        }).await.unwrap();

        let result = db.delete_remote_entry_by_path(mount_id, "uploading.txt").await.unwrap();
        assert!(result.is_none());
        assert!(db.get_by_inode(inode).await.unwrap().is_some());
    }

    // ── Base version tests ──────────────────────────────────────────────

    #[tokio::test]
    async fn base_hash_round_trip() {
        let (db, mount_id, root) = setup_db_with_mount().await;

        let inode = db.insert_file(&NewFileEntry {
            mount_id,
            parent: root,
            name: "notes.txt".into(),
            remote_path: "notes.txt".into(),
            kind: FileKind::File,
            size: 100,
            mtime: SystemTime::now(),
            etag: Some("etag1".into()),
            status: SyncStatus::Cached,
            cache_path: None,
            cache_size: None,
        }).await.unwrap();

        // Initially no base
        assert!(db.get_base_hash(inode, mount_id).await.unwrap().is_none());

        // Store a base hash
        db.set_base_hash(inode, mount_id, "abcdef1234567890", 100).await.unwrap();
        assert_eq!(
            db.get_base_hash(inode, mount_id).await.unwrap().unwrap(),
            "abcdef1234567890"
        );

        // Overwrite with new hash
        db.set_base_hash(inode, mount_id, "fedcba0987654321", 200).await.unwrap();
        assert_eq!(
            db.get_base_hash(inode, mount_id).await.unwrap().unwrap(),
            "fedcba0987654321"
        );

        // Remove
        db.remove_base_hash(inode, mount_id).await.unwrap();
        assert!(db.get_base_hash(inode, mount_id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn remove_base_hash_nonexistent() {
        let (db, mount_id, _) = setup_db_with_mount().await;
        // Removing a nonexistent base hash should not error
        db.remove_base_hash(999, mount_id).await.unwrap();
    }

    #[tokio::test]
    async fn base_hash_ref_count_tracks_references() {
        let (db, mount_id, root) = setup_db_with_mount().await;

        let inode1 = db.insert_file(&NewFileEntry {
            mount_id,
            parent: root,
            name: "a.txt".into(),
            remote_path: "a.txt".into(),
            kind: FileKind::File,
            size: 50,
            mtime: SystemTime::now(),
            etag: None,
            status: SyncStatus::Cached,
            cache_path: None,
            cache_size: None,
        }).await.unwrap();

        let inode2 = db.insert_file(&NewFileEntry {
            mount_id,
            parent: root,
            name: "b.txt".into(),
            remote_path: "b.txt".into(),
            kind: FileKind::File,
            size: 50,
            mtime: SystemTime::now(),
            etag: None,
            status: SyncStatus::Cached,
            cache_path: None,
            cache_size: None,
        }).await.unwrap();

        let hash = "same_content_hash";
        db.set_base_hash(inode1, mount_id, hash, 50).await.unwrap();
        db.set_base_hash(inode2, mount_id, hash, 50).await.unwrap();
        assert_eq!(db.base_hash_ref_count(hash).await.unwrap(), 2);

        db.remove_base_hash(inode1, mount_id).await.unwrap();
        assert_eq!(db.base_hash_ref_count(hash).await.unwrap(), 1);

        db.remove_base_hash(inode2, mount_id).await.unwrap();
        assert_eq!(db.base_hash_ref_count(hash).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn stale_base_entries_respects_dirty_status() {
        let (db, mount_id, root) = setup_db_with_mount().await;

        let inode = db.insert_file(&NewFileEntry {
            mount_id,
            parent: root,
            name: "dirty.txt".into(),
            remote_path: "dirty.txt".into(),
            kind: FileKind::File,
            size: 50,
            mtime: SystemTime::now(),
            etag: None,
            status: SyncStatus::Dirty,
            cache_path: None,
            cache_size: None,
        }).await.unwrap();

        db.set_base_hash(inode, mount_id, "hash123", 50).await.unwrap();

        // Even with max_age_days=0, dirty files should not appear
        let stale = db.stale_base_entries(mount_id, 0, 100).await.unwrap();
        assert!(stale.is_empty(), "dirty files should not be evicted");
    }

    #[tokio::test]
    async fn stale_base_entries_returns_old_cached_entries() {
        let (db, mount_id, root) = setup_db_with_mount().await;

        let inode = db.insert_file(&NewFileEntry {
            mount_id,
            parent: root,
            name: "old.txt".into(),
            remote_path: "old.txt".into(),
            kind: FileKind::File,
            size: 50,
            mtime: SystemTime::now(),
            etag: None,
            status: SyncStatus::Cached,
            cache_path: None,
            cache_size: None,
        }).await.unwrap();

        db.set_base_hash(inode, mount_id, "oldhash", 50).await.unwrap();

        // With max_age_days=0, the entry was just created so updated_at ~= now.
        // It should NOT be returned because updated_at >= now - 0 days.
        // Use a very large max_age_days to confirm it's excluded when fresh.
        let stale = db.stale_base_entries(mount_id, 365, 100).await.unwrap();
        assert!(stale.is_empty(), "fresh entries should not be stale");

        // Manually backdate the updated_at to simulate an old entry
        {
            let conn = db.conn.lock().await;
            conn.execute(
                "UPDATE base_versions SET updated_at = unixepoch() - 100*86400 WHERE inode=?1",
                params![inode as i64],
            ).unwrap();
        }

        let stale = db.stale_base_entries(mount_id, 30, 100).await.unwrap();
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].0, inode);
        assert_eq!(stale[0].1, "oldhash");
    }

    #[tokio::test]
    async fn base_version_cascade_deletes() {
        let (db, mount_id, root) = setup_db_with_mount().await;

        let inode = db.insert_file(&NewFileEntry {
            mount_id,
            parent: root,
            name: "del.txt".into(),
            remote_path: "del.txt".into(),
            kind: FileKind::File,
            size: 50,
            mtime: SystemTime::now(),
            etag: None,
            status: SyncStatus::Remote,
            cache_path: None,
            cache_size: None,
        }).await.unwrap();

        db.set_base_hash(inode, mount_id, "hash_del", 50).await.unwrap();
        assert!(db.get_base_hash(inode, mount_id).await.unwrap().is_some());

        // Deleting the file_index entry should cascade to base_versions
        db.delete_entry(inode).await.unwrap();
        assert!(db.get_base_hash(inode, mount_id).await.unwrap().is_none());
    }
}

impl StateDb {
    /// Look up a file_index entry by its local cache path.
    pub async fn get_by_cache_path(
        &self,
        mount_id:   u32,
        cache_path: &std::path::Path,
    ) -> Result<Option<FileEntry>> {
        let path_str = match cache_path.to_str() {
            Some(s) => s.to_owned(),
            None    => return Ok(None),
        };
        let conn = self.conn.lock().await;
        conn.query_row(
            "SELECT inode, mount_id, parent_inode, name, remote_path, kind,
                    size, mtime, etag, status, cache_path, cache_size, dir_listed
             FROM file_index WHERE mount_id=?1 AND cache_path=?2",
            params![mount_id, path_str],
            row_to_entry,
        ).optional().map_err(Into::into)
    }

    /// Look up a file_index entry by its remote path.
    pub async fn get_by_remote_path(
        &self,
        mount_id:    u32,
        remote_path: &str,
    ) -> Result<Option<FileEntry>> {
        let conn = self.conn.lock().await;
        conn.query_row(
            "SELECT inode, mount_id, parent_inode, name, remote_path, kind,
                    size, mtime, etag, status, cache_path, cache_size, dir_listed
             FROM file_index WHERE mount_id=?1 AND remote_path=?2",
            params![mount_id, remote_path],
            row_to_entry,
        ).optional().map_err(Into::into)
    }

    pub async fn fail_queue_job_by_inode(
        &self,
        inode:     Inode,
        error_msg: &str,
        backoff_s: u64,
    ) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "UPDATE sync_queue
             SET attempt=attempt+1, error_msg=?1, next_attempt=unixepoch()+?2
             WHERE inode=?3",
            params![error_msg, backoff_s as i64, inode as i64],
        )?;
        conn.execute(
            "UPDATE file_index SET status='dirty' WHERE inode=?1",
            params![inode as i64],
        )?;
        Ok(())
    }

    pub async fn delete_entry(&self, inode: Inode) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute("DELETE FROM file_index WHERE inode=?1", params![inode as i64])?;
        Ok(())
    }

    pub async fn rename_entry(
        &self,
        inode:      Inode,
        new_parent: Inode,
        new_name:   &str,
        new_remote: &str,
        new_cache_path: Option<&std::path::Path>,
    ) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "UPDATE file_index SET parent_inode=?1, name=?2, remote_path=?3, cache_path=?4
             WHERE inode=?5",
            params![
                new_parent as i64, new_name, new_remote,
                new_cache_path.and_then(|p| p.to_str()),
                inode as i64,
            ],
        )?;
        Ok(())
    }

    pub async fn raw_conn(&self)
        -> tokio::sync::MutexGuard<'_, rusqlite::Connection>
    {
        self.conn.lock().await
    }

    pub async fn invalidate_dir(&self, inode: Inode) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "UPDATE file_index SET dir_listed=NULL WHERE inode=?1",
            params![inode as i64],
        )?;
        Ok(())
    }

    // ── Tombstones ─────────────────────────────────────────────────────────────

    /// Record a tombstone so the poller skips this path until the remote
    /// delete completes or the TTL expires.
    pub async fn insert_tombstone(
        &self,
        mount_id:    u32,
        remote_path: &str,
        ttl_secs:    u64,
    ) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT OR REPLACE INTO delete_tombstones (mount_id, remote_path, expires_at)
             VALUES (?1, ?2, unixepoch() + ?3)",
            params![mount_id, remote_path, ttl_secs as i64],
        )?;
        Ok(())
    }

    /// Remove a tombstone after successful remote deletion.
    pub async fn remove_tombstone(&self, mount_id: u32, remote_path: &str) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "DELETE FROM delete_tombstones WHERE mount_id = ?1 AND remote_path = ?2",
            params![mount_id, remote_path],
        )?;
        Ok(())
    }

    /// Check if a path is tombstoned (exact match or under a tombstoned directory).
    pub async fn is_tombstoned(&self, mount_id: u32, remote_path: &str) -> Result<bool> {
        let conn = self.conn.lock().await;
        let found: bool = conn.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM delete_tombstones
                WHERE mount_id = ?1
                  AND expires_at > unixepoch()
                  AND (?2 = remote_path OR ?2 LIKE remote_path || '/%')
            )",
            params![mount_id, remote_path],
            |r| r.get(0),
        )?;
        Ok(found)
    }

    /// Load all active tombstone paths for a mount into memory.
    /// Used by the poller to batch-check without per-file DB queries.
    pub async fn active_tombstones(&self, mount_id: u32) -> Result<Vec<String>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT remote_path FROM delete_tombstones
             WHERE mount_id = ?1 AND expires_at > unixepoch()",
        )?;
        let paths = stmt.query_map(params![mount_id], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<String>>>()?;
        Ok(paths)
    }

    /// Delete expired tombstones.
    pub async fn cleanup_expired_tombstones(&self) -> Result<usize> {
        let conn = self.conn.lock().await;
        let n = conn.execute(
            "DELETE FROM delete_tombstones WHERE expires_at <= unixepoch()",
            [],
        )?;
        Ok(n)
    }

    // ── Poll generation / diffing ───────────────────────────────────────────

    /// Load all file_index rows for a mount into memory for diffing.
    pub async fn snapshot_remote_index(
        &self,
        mount_id: u32,
    ) -> Result<HashMap<String, RemoteSnapshot>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT remote_path, inode, etag, size, status
             FROM file_index WHERE mount_id = ?1",
        )?;
        let rows = stmt.query_map(params![mount_id], |r| {
            let status_str: String = r.get(4)?;
            Ok((
                r.get::<_, String>(0)?,
                RemoteSnapshot {
                    inode:  r.get::<_, i64>(1)? as u64,
                    etag:   r.get(2)?,
                    size:   r.get::<_, i64>(3)? as u64,
                    status: SyncStatus::from_str(&status_str).unwrap_or(SyncStatus::Remote),
                },
            ))
        })?;
        let mut map = HashMap::new();
        for row in rows {
            let (path, snap) = row?;
            map.insert(path, snap);
        }
        Ok(map)
    }

    /// Update poll_generation for a batch of inodes in a single transaction.
    pub async fn batch_mark_generation(
        &self,
        inodes:     &[Inode],
        generation: u64,
    ) -> Result<()> {
        if inodes.is_empty() { return Ok(()); }
        // Process in chunks to avoid holding the mutex for too long
        const CHUNK_SIZE: usize = 1000;
        for chunk in inodes.chunks(CHUNK_SIZE) {
            let conn = self.conn.lock().await;
            conn.execute_batch("BEGIN")?;
            let result: Result<()> = (|| {
                for &inode in chunk {
                    conn.execute(
                        "UPDATE file_index SET poll_generation = ?1 WHERE inode = ?2",
                        params![generation as i64, inode as i64],
                    )?;
                }
                Ok(())
            })();
            match &result {
                Ok(()) => conn.execute_batch("COMMIT")?,
                Err(_) => { let _ = conn.execute_batch("ROLLBACK"); }
            }
            result?;
        }
        Ok(())
    }

    /// Delete entries with old poll_generation (absent from remote listing).
    /// Preserves dirty/uploading entries (local changes not yet synced).
    /// Returns (inode, remote_path, cache_path) for cache cleanup.
    pub async fn delete_stale_entries(
        &self,
        mount_id:   u32,
        generation: u64,
    ) -> Result<Vec<(Inode, String, Option<PathBuf>)>> {
        let conn = self.conn.lock().await;
        // Find stale entries
        let mut stmt = conn.prepare(
            "SELECT inode, remote_path, cache_path FROM file_index
             WHERE mount_id = ?1
               AND poll_generation < ?2
               AND status NOT IN ('dirty', 'uploading')
               AND inode != ?3",
        )?;
        let stale: Vec<(Inode, String, Option<PathBuf>)> = stmt.query_map(
            params![mount_id, generation as i64, FUSE_ROOT_INODE as i64],
            |r| Ok((
                r.get::<_, i64>(0)? as u64,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?.map(PathBuf::from),
            )),
        )?.collect::<rusqlite::Result<Vec<_>>>()?;

        // Delete them
        for &(inode, _, _) in &stale {
            conn.execute(
                "DELETE FROM file_index WHERE inode = ?1",
                params![inode as i64],
            )?;
        }
        Ok(stale)
    }

    pub async fn get_poll_generation(&self, mount_id: u32) -> Result<u64> {
        let conn = self.conn.lock().await;
        let gen: Option<String> = conn.query_row(
            "SELECT token FROM change_tokens WHERE mount_id = ?1",
            params![mount_id],
            |r| r.get(0),
        ).optional()?;
        Ok(gen.and_then(|s| s.parse().ok()).unwrap_or(0))
    }

    pub async fn set_poll_generation(&self, mount_id: u32, generation: u64) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO change_tokens (mount_id, token, updated_at)
             VALUES (?1, ?2, unixepoch())
             ON CONFLICT(mount_id) DO UPDATE SET token=excluded.token, updated_at=excluded.updated_at",
            params![mount_id, generation.to_string()],
        )?;
        Ok(())
    }

    // ── Change token methods (for delta polling) ────────────────────────────

    /// Get the raw change token string for a mount (e.g. Google Drive pageToken).
    /// Returns `None` if no token has been stored yet.
    pub async fn get_change_token(&self, mount_id: u32) -> Result<Option<String>> {
        let conn = self.conn.lock().await;
        let token: Option<String> = conn.query_row(
            "SELECT token FROM change_tokens WHERE mount_id = ?1",
            params![mount_id],
            |r| r.get(0),
        ).optional()?;
        Ok(token)
    }

    /// Store a change token for a mount (upserts).
    pub async fn set_change_token(&self, mount_id: u32, token: &str) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO change_tokens (mount_id, token, updated_at)
             VALUES (?1, ?2, unixepoch())
             ON CONFLICT(mount_id) DO UPDATE SET token=excluded.token, updated_at=excluded.updated_at",
            params![mount_id, token],
        )?;
        Ok(())
    }

    /// Clear the stored change token for a mount (used on token invalidation).
    pub async fn clear_change_token(&self, mount_id: u32) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "DELETE FROM change_tokens WHERE mount_id = ?1",
            params![mount_id],
        )?;
        Ok(())
    }

    // ── Base version methods (for 3-way merge) ────────────────────────────

    /// Record the object hash of the last known-good base version for a file.
    pub async fn set_base_hash(
        &self,
        inode:    Inode,
        mount_id: u32,
        hash:     &str,
        size:     u64,
    ) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO base_versions (inode, mount_id, object_hash, file_size, updated_at)
             VALUES (?1, ?2, ?3, ?4, unixepoch())
             ON CONFLICT(inode, mount_id) DO UPDATE SET
               object_hash=excluded.object_hash,
               file_size=excluded.file_size,
               updated_at=unixepoch()",
            params![inode as i64, mount_id, hash, size as i64],
        )?;
        Ok(())
    }

    /// Look up the base version object hash for a file.
    pub async fn get_base_hash(
        &self,
        inode:    Inode,
        mount_id: u32,
    ) -> Result<Option<String>> {
        let conn = self.conn.lock().await;
        let hash: Option<String> = conn.query_row(
            "SELECT object_hash FROM base_versions WHERE inode=?1 AND mount_id=?2",
            params![inode as i64, mount_id],
            |r| r.get(0),
        ).optional()?;
        Ok(hash)
    }

    /// Remove the base version mapping for a file.
    pub async fn remove_base_hash(&self, inode: Inode, mount_id: u32) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "DELETE FROM base_versions WHERE inode=?1 AND mount_id=?2",
            params![inode as i64, mount_id],
        )?;
        Ok(())
    }

    /// Count how many base_versions rows reference a given object hash.
    /// Used to decide whether it's safe to delete the blob file.
    pub async fn base_hash_ref_count(&self, hash: &str) -> Result<u64> {
        let conn = self.conn.lock().await;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM base_versions WHERE object_hash=?1",
            params![hash],
            |r| r.get(0),
        )?;
        Ok(count as u64)
    }

    /// Find base version entries that are stale and eligible for eviction.
    /// Returns entries where the corresponding file is not dirty/uploading
    /// and the base was last updated more than `max_age_days` ago.
    pub async fn stale_base_entries(
        &self,
        mount_id:     u32,
        max_age_days: u32,
        limit:        u32,
    ) -> Result<Vec<(Inode, String, u64)>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT b.inode, b.object_hash, b.file_size
             FROM base_versions b
             JOIN file_index f ON f.inode = b.inode AND f.mount_id = b.mount_id
             WHERE b.mount_id = ?1
               AND f.status NOT IN ('dirty', 'uploading')
               AND b.updated_at < unixepoch() - (?2 * 86400)
             ORDER BY b.updated_at ASC
             LIMIT ?3",
        )?;
        let rows = stmt
            .query_map(
                params![mount_id, max_age_days, limit],
                |r| Ok((
                    r.get::<_, i64>(0)? as u64,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)? as u64,
                )),
            )?
            .filter_map(|r| r.ok())
            .collect();
        Ok(rows)
    }

    /// Delete a single file_index entry by remote path.
    /// Preserves dirty/uploading entries (local changes not yet synced).
    /// Returns the (inode, cache_path) of the deleted entry for cache cleanup,
    /// or `None` if the entry didn't exist or was protected.
    pub async fn delete_remote_entry_by_path(
        &self,
        mount_id: u32,
        remote_path: &str,
    ) -> Result<Option<(Inode, Option<PathBuf>)>> {
        let conn = self.conn.lock().await;
        let row: Option<(Inode, Option<PathBuf>)> = conn.query_row(
            "SELECT inode, cache_path FROM file_index
             WHERE mount_id = ?1 AND remote_path = ?2
               AND status NOT IN ('dirty', 'uploading')",
            params![mount_id, remote_path],
            |r| Ok((
                r.get::<_, i64>(0)? as u64,
                r.get::<_, Option<String>>(1)?.map(PathBuf::from),
            )),
        ).optional()?;

        if let Some((inode, _)) = &row {
            conn.execute(
                "DELETE FROM file_index WHERE inode = ?1",
                params![*inode as i64],
            )?;
        }

        Ok(row)
    }

    /// Delete all file_index and related entries for a mount.
    /// Used to clear corrupt state (e.g. stale DB from a crashed run).
    /// Resets the autoincrement counter so the root can be re-inserted
    /// at inode 1 (required for FUSE_ROOT_INODE self-referencing FK).
    pub async fn delete_mount_entries(&self, mount_id: u32) -> Result<()> {
        let conn = self.conn.lock().await;
        // cache_lru and sync_queue have ON DELETE CASCADE from file_index
        conn.execute(
            "DELETE FROM file_index WHERE mount_id = ?1",
            params![mount_id],
        )?;
        // Reset autoincrement so the next insert gets inode 1 again.
        // The root entry uses parent_inode=1 (self-referencing), which
        // only satisfies the FK if the row itself gets inode 1.
        let remaining: i64 = conn.query_row(
            "SELECT COUNT(*) FROM file_index", [], |r| r.get(0),
        )?;
        if remaining == 0 {
            conn.execute(
                "DELETE FROM sqlite_sequence WHERE name = 'file_index'",
                [],
            )?;
        }
        info!(mount_id, "cleared all file entries for mount");
        Ok(())
    }
}
