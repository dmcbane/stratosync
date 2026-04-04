/// SQLite state database — the daemon's persistent store.
///
/// See docs/architecture/04-state-db.md for full schema documentation.
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use tokio::sync::Mutex;
use tracing::{debug, info};

use crate::types::{FileEntry, FileKind, Inode, SyncStatus, FUSE_ROOT_INODE};

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
        mount_id: u32,
        parent:   Inode,
        entries:  &[(String, String, FileKind, u64, SystemTime, Option<String>)],
    ) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute_batch("BEGIN")?;
        let result: Result<()> = (|| {
            for (name, remote_path, kind, size, mtime, etag) in entries {
                conn.execute(
                    "INSERT INTO file_index
                       (mount_id, parent_inode, name, remote_path, kind, size, mtime, etag, status)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'remote')
                     ON CONFLICT(mount_id, remote_path) DO UPDATE SET
                       parent_inode = excluded.parent_inode,
                       name         = excluded.name,
                       kind         = excluded.kind,
                       size         = excluded.size,
                       mtime        = excluded.mtime,
                       etag         = excluded.etag,
                       status       = CASE
                         WHEN status IN ('dirty','uploading') THEN status
                         ELSE 'stale'
                       END",
                    params![
                        mount_id, parent as i64, name, remote_path,
                        kind.as_str(), *size as i64, to_unix(*mtime), etag.as_deref(),
                    ],
                )?;
            }
            Ok(())
        })();
        match &result {
            Ok(()) => conn.execute_batch("COMMIT")?,
            Err(_) => { let _ = conn.execute_batch("ROLLBACK"); }
        }
        result
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
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO file_index
               (mount_id, parent_inode, name, remote_path, kind, size, mtime, etag, status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'remote')
             ON CONFLICT(mount_id, remote_path) DO UPDATE SET
               parent_inode = excluded.parent_inode,
               name         = excluded.name,
               kind         = excluded.kind,
               size         = excluded.size,
               mtime        = excluded.mtime,
               etag         = excluded.etag,
               status       = CASE
                 WHEN status IN ('dirty','uploading') THEN status  -- don't clobber
                 ELSE 'stale'
               END",
            params![
                mount_id, parent, name, remote_path,
                kind.as_str(), size as i64, to_unix(mtime), etag,
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
        parent: Inode,
        name:   &str,
    ) -> Result<Option<FileEntry>> {
        let conn = self.conn.lock().await;
        conn.query_row(
            "SELECT inode, mount_id, parent_inode, name, remote_path, kind,
                    size, mtime, etag, status, cache_path, cache_size, dir_listed
             FROM file_index WHERE parent_inode = ?1 AND name = ?2",
            params![parent as i64, name],
            row_to_entry,
        ).optional().map_err(Into::into)
    }

    pub async fn list_children(&self, parent: Inode) -> Result<Vec<FileEntry>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT inode, mount_id, parent_inode, name, remote_path, kind,
                    size, mtime, etag, status, cache_path, cache_size, dir_listed
             FROM file_index WHERE parent_inode = ?1
             ORDER BY inode ASC",
        )?;
        let rows = stmt.query_map(params![parent as i64], row_to_entry)?
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
