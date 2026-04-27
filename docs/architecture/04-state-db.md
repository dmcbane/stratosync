# State Database Schema

SQLite in WAL mode is the single source of truth for all daemon state.
**One database file per mount**, named after the mount, under
`~/.local/share/stratosync/<mount_name>.db` (XDG_DATA_HOME). Per-mount
databases give complete isolation of inode namespaces between mounts —
this also happens to be the substrate that makes "multiple accounts of
the same provider" work without any daemon-side flag.

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
    poll_generation INTEGER NOT NULL DEFAULT 0,  -- migration 0003: stale-entry sweep
    UNIQUE(mount_id, remote_path)
);

CREATE INDEX idx_file_index_parent     ON file_index(parent_inode);
CREATE INDEX idx_file_index_status     ON file_index(status);
CREATE INDEX idx_file_index_mount      ON file_index(mount_id, remote_path);
CREATE INDEX idx_file_index_generation ON file_index(mount_id, poll_generation);
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

### `delete_tombstones` (migration 0002)

Prevents the poller from re-adding files that were just deleted locally
but haven't been removed from the remote yet (the remote delete is async).
Tombstones expire after 5 minutes as a safety net.

```sql
CREATE TABLE delete_tombstones (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    mount_id    INTEGER NOT NULL REFERENCES mounts(id),
    remote_path TEXT    NOT NULL,
    created_at  INTEGER NOT NULL DEFAULT (unixepoch()),
    expires_at  INTEGER NOT NULL,
    UNIQUE(mount_id, remote_path)
);

CREATE INDEX idx_tombstones_mount_path ON delete_tombstones(mount_id, remote_path);
CREATE INDEX idx_tombstones_expires    ON delete_tombstones(expires_at);
```

Directory tombstones block children via prefix match. The poller cleans
up expired tombstones at the start of each cycle.

### `change_tokens`

Provider-specific delta tokens for efficient remote change detection.

```sql
CREATE TABLE change_tokens (
    mount_id     INTEGER PRIMARY KEY REFERENCES mounts(id),
    token        TEXT    NOT NULL,   -- pageToken (GDrive), deltaLink (OneDrive), etc.
    updated_at   INTEGER NOT NULL DEFAULT (unixepoch())
);
```

### `base_versions` (migration 0004)

Per-file pointer to the "last known good" content used as the common
ancestor in 3-way text merges. The actual content lives in the
content-addressed `BaseStore` at `<cache_dir>/.bases/objects/`.

```sql
CREATE TABLE base_versions (
    inode       INTEGER NOT NULL,
    mount_id    INTEGER NOT NULL REFERENCES mounts(id),
    object_hash TEXT    NOT NULL,    -- SHA-256
    file_size   INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL DEFAULT (unixepoch()),
    PRIMARY KEY (inode, mount_id),
    FOREIGN KEY (inode) REFERENCES file_index(inode) ON DELETE CASCADE
);

CREATE INDEX idx_base_versions_hash ON base_versions(object_hash);
CREATE INDEX idx_base_versions_age  ON base_versions(updated_at);
```

Bases are captured on hydration and after each successful upload (subject
to the size and extension allowlist in `[daemon.sync]`). Stale bases are
evicted by the cache manager based on `base_retention_days`.

### `version_history` (migration 0005)

Append-only file-version history for the Phase 5 versioning feature.
Inserted at two points by the sync engine: (a) the poller, just before a
remote change replaces local content (`source = 'before_poll'`); (b) the
upload queue, after a successful upload (`source = 'after_upload'`).

```sql
CREATE TABLE version_history (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    inode       INTEGER NOT NULL,
    mount_id    INTEGER NOT NULL REFERENCES mounts(id),
    object_hash TEXT    NOT NULL,    -- SHA-256, references BaseStore blob
    recorded_at INTEGER NOT NULL DEFAULT (unixepoch()),
    file_size   INTEGER NOT NULL,
    etag        TEXT,
    source      TEXT    NOT NULL CHECK (source IN (
                    'before_poll','after_upload','manual'
                )),
    FOREIGN KEY (inode) REFERENCES file_index(inode) ON DELETE CASCADE
);
```

The blob storage is **shared** with `base_versions` — both reference
content-addressed objects in `<cache_dir>/.bases/objects/`. A blob is
removed from disk only when **no `version_history` row and no
`base_versions` row** references it (cross-table refcount). This means a
blob that's serving as a 3-way-merge base is automatically retained even
after it scrolls out of `version_retention`, and vice versa.

Per-file pruning to `version_retention` rows happens immediately after
each insert. `version_retention = 0` disables the feature entirely (the
sync engine skips the snapshot call).

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

All schema changes go through versioned migrations under
`crates/stratosync-core/src/state/migrations/`. The daemon checks the current
schema version at startup and applies pending migrations in order. Migrations
are embedded as `include_str!` SQL strings.

Current migrations (each opens a per-mount DB):

| ID | File | What it adds |
|----|------|--------------|
| 0001 | `0001_initial.sql` | `mounts`, `file_index`, `sync_queue`, `change_tokens`, `cache_lru`, `xattr_store`, `schema_migrations` |
| 0002 | `0002_delete_tombstones.sql` | `delete_tombstones` table — see above |
| 0003 | `0003_poll_generation.sql` | `file_index.poll_generation` column + index for the stale-entry sweep |
| 0004 | `0004_base_versions.sql` | `base_versions` table for 3-way merge bases |
| 0005 | `0005_version_history.sql` | `version_history` table for Phase 5 file versioning |

Migrations must be idempotent and forward-only (no down migrations for
production use).

---

## Notes on Nextcloud Desktop Client's Journal DB

Nextcloud's `_sync_XXXX.db` (also SQLite) informed several design choices here:
- They store a `phash` (path hash) as the primary key for fast lookup by remote path.
- They use a separate `uploadinfo` table for in-progress chunked uploads, which survives restarts.
- They track a `checksum` (provider-specific: SHA1 for Nextcloud, etag for others) per file separately from mtime to detect silent data corruption.

We adopt the checksum-vs-mtime distinction in `file_index.etag` and will add an optional `checksum` column for backends that provide content hashes (Nextcloud, rclone `--checksum` mode).
