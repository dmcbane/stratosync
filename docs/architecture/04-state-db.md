# State Database Schema

SQLite in WAL mode is the single source of truth for all daemon state. The DB lives at `~/.local/share/stratosync/state.db` (XDG_DATA_HOME).

## Connection Settings

```sql
PRAGMA journal_mode = WAL;        -- concurrent readers + 1 writer
PRAGMA synchronous   = NORMAL;    -- fsync on WAL checkpoint only (safe with WAL)
PRAGMA foreign_keys  = ON;
PRAGMA auto_vacuum   = INCREMENTAL;
PRAGMA busy_timeout  = 5000;      -- wait up to 5s on lock contention
```

---

## Tables

### `mounts`

One row per configured mount point.

```sql
CREATE TABLE mounts (
    id           INTEGER PRIMARY KEY,
    name         TEXT    NOT NULL UNIQUE,  -- user-defined label, e.g. "gdrive"
    remote       TEXT    NOT NULL,         -- rclone remote spec, e.g. "gdrive:/"
    mount_path   TEXT    NOT NULL UNIQUE,  -- local path, e.g. "/home/dale/GoogleDrive"
    cache_dir    TEXT    NOT NULL,         -- e.g. "~/.cache/stratosync/gdrive"
    cache_quota  INTEGER NOT NULL DEFAULT 5368709120,  -- bytes, default 5 GiB
    poll_secs    INTEGER NOT NULL DEFAULT 60,
    created_at   INTEGER NOT NULL DEFAULT (unixepoch()),
    active       INTEGER NOT NULL DEFAULT 1  -- 0 = disabled
);
```

### `file_index`

The canonical record of every known file and directory.

```sql
CREATE TABLE file_index (
    inode        INTEGER PRIMARY KEY,
    mount_id     INTEGER NOT NULL REFERENCES mounts(id),
    parent_inode INTEGER REFERENCES file_index(inode),
    name         TEXT    NOT NULL,
    remote_path  TEXT    NOT NULL,          -- full rclone path
    kind         TEXT    NOT NULL CHECK(kind IN ('file','dir','symlink')),
    size         INTEGER NOT NULL DEFAULT 0,
    mtime        INTEGER NOT NULL DEFAULT 0,  -- unix epoch, seconds
    etag         TEXT,                        -- remote ETag/hash (provider-specific)
    status       TEXT    NOT NULL DEFAULT 'remote'
                         CHECK(status IN (
                             'remote','hydrating','cached',
                             'dirty','uploading','stale','conflict'
                         )),
    cache_path   TEXT,    -- absolute path to cached file (NULL if not hydrated)
    cache_size   INTEGER, -- bytes currently in cache for this file
    cache_mtime  INTEGER, -- mtime of the cache file when last validated
    dir_listed   INTEGER, -- unixepoch() when this dir was last listed; NULL if not yet
    UNIQUE(mount_id, remote_path)
);

CREATE INDEX idx_file_index_parent ON file_index(parent_inode);
CREATE INDEX idx_file_index_status ON file_index(status);
CREATE INDEX idx_file_index_mount  ON file_index(mount_id, remote_path);
```

**Auto-increment inode**: SQLite's `INTEGER PRIMARY KEY` auto-increments. Inodes are reused only if explicitly deleted and the row is purged — we never reuse inodes for the life of a DB file.

### `sync_queue`

Persistent work queue for uploads and remote mutations. Survives daemon restarts.

```sql
CREATE TABLE sync_queue (
    id           INTEGER PRIMARY KEY,
    inode        INTEGER REFERENCES file_index(inode),
    mount_id     INTEGER NOT NULL REFERENCES mounts(id),
    op           TEXT    NOT NULL CHECK(op IN (
                     'upload','delete','mkdir','rmdir','rename','move'
                 )),
    remote_path  TEXT    NOT NULL,
    remote_dest  TEXT,    -- for rename/move
    known_etag   TEXT,    -- optimistic lock value
    priority     INTEGER NOT NULL DEFAULT 1,
    attempt      INTEGER NOT NULL DEFAULT 0,
    next_attempt INTEGER NOT NULL DEFAULT 0,  -- unixepoch() for retry
    created_at   INTEGER NOT NULL DEFAULT (unixepoch()),
    error_msg    TEXT     -- last error, for diagnostics
);

CREATE INDEX idx_sync_queue_due ON sync_queue(next_attempt, priority DESC);
```

### `change_tokens`

Provider-specific delta tokens for efficient remote change detection.

```sql
CREATE TABLE change_tokens (
    mount_id     INTEGER PRIMARY KEY REFERENCES mounts(id),
    token        TEXT    NOT NULL,   -- pageToken (GDrive), deltaLink (OneDrive), etc.
    updated_at   INTEGER NOT NULL DEFAULT (unixepoch())
);
```

### `cache_lru`

Tracks cache access times for LRU eviction.

```sql
CREATE TABLE cache_lru (
    inode        INTEGER PRIMARY KEY REFERENCES file_index(inode),
    last_access  INTEGER NOT NULL DEFAULT (unixepoch()),
    pinned       INTEGER NOT NULL DEFAULT 0  -- 1 = never evict
);

CREATE INDEX idx_cache_lru_access ON cache_lru(last_access) WHERE pinned = 0;
```

### `xattr_store`

Extended attribute values for user-visible sync metadata and future app use.

```sql
CREATE TABLE xattr_store (
    inode        INTEGER NOT NULL REFERENCES file_index(inode),
    name         TEXT    NOT NULL,   -- e.g. "user.stratosync.status"
    value        BLOB    NOT NULL,
    PRIMARY KEY (inode, name)
);
```

### `schema_migrations`

```sql
CREATE TABLE schema_migrations (
    version    INTEGER PRIMARY KEY,
    applied_at INTEGER NOT NULL DEFAULT (unixepoch()),
    description TEXT
);
```

---

## Key Queries

### Get next upload job

```sql
SELECT q.*, f.cache_path, f.remote_path
FROM sync_queue q
JOIN file_index f ON f.inode = q.inode
WHERE q.op = 'upload'
  AND q.next_attempt <= unixepoch()
ORDER BY q.priority DESC, q.created_at ASC
LIMIT 1;
```

### LRU eviction candidates (evict until under quota)

```sql
SELECT f.inode, f.cache_path, f.cache_size
FROM file_index f
JOIN cache_lru l ON l.inode = f.inode
WHERE f.status = 'cached'
  AND f.mount_id = ?
  AND l.pinned = 0
ORDER BY l.last_access ASC;
```

### Files needing re-hydration after restart

```sql
UPDATE file_index
SET status = 'remote', cache_path = NULL
WHERE status = 'hydrating';

-- Also remove partial cache files (done in application layer)
```

### Snapshot remote metadata for diff

```sql
SELECT remote_path, etag, mtime, size
FROM file_index
WHERE mount_id = ?
  AND kind = 'file';
```

---

## Migration Strategy

All schema changes go through versioned migrations in `src/state/migrations/`. The daemon checks the current schema version at startup and applies pending migrations in order. Migrations are embedded as `include_str!` SQL strings:

```rust
const MIGRATIONS: &[(&str, &str)] = &[
    ("0001", include_str!("migrations/0001_initial.sql")),
    ("0002", include_str!("migrations/0002_add_change_tokens.sql")),
    // ...
];
```

Migrations must be idempotent and forward-only (no down migrations for production use).

---

## Notes on Nextcloud Desktop Client's Journal DB

Nextcloud's `_sync_XXXX.db` (also SQLite) informed several design choices here:
- They store a `phash` (path hash) as the primary key for fast lookup by remote path.
- They use a separate `uploadinfo` table for in-progress chunked uploads, which survives restarts.
- They track a `checksum` (provider-specific: SHA1 for Nextcloud, etag for others) per file separately from mtime to detect silent data corruption.

We adopt the checksum-vs-mtime distinction in `file_index.etag` and will add an optional `checksum` column for backends that provide content hashes (Nextcloud, rclone `--checksum` mode).
