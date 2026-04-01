-- Migration 0001: Initial schema
-- See docs/architecture/04-state-db.md for full documentation

CREATE TABLE mounts (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    name         TEXT    NOT NULL UNIQUE,
    remote       TEXT    NOT NULL,
    mount_path   TEXT    NOT NULL UNIQUE,
    cache_dir    TEXT    NOT NULL,
    cache_quota  INTEGER NOT NULL DEFAULT 5368709120,
    poll_secs    INTEGER NOT NULL DEFAULT 60,
    created_at   INTEGER NOT NULL DEFAULT (unixepoch()),
    active       INTEGER NOT NULL DEFAULT 1
);

CREATE TABLE file_index (
    inode        INTEGER PRIMARY KEY AUTOINCREMENT,
    mount_id     INTEGER NOT NULL REFERENCES mounts(id),
    parent_inode INTEGER REFERENCES file_index(inode),
    name         TEXT    NOT NULL,
    remote_path  TEXT    NOT NULL,
    kind         TEXT    NOT NULL CHECK(kind IN ('file','dir','symlink')),
    size         INTEGER NOT NULL DEFAULT 0,
    mtime        INTEGER NOT NULL DEFAULT 0,
    etag         TEXT,
    status       TEXT    NOT NULL DEFAULT 'remote'
                         CHECK(status IN (
                             'remote','hydrating','cached',
                             'dirty','uploading','stale','conflict'
                         )),
    cache_path   TEXT,
    cache_size   INTEGER,
    cache_mtime  INTEGER,
    dir_listed   INTEGER,
    UNIQUE(mount_id, remote_path)
);

CREATE INDEX idx_file_index_parent ON file_index(parent_inode);
CREATE INDEX idx_file_index_status ON file_index(status);
CREATE INDEX idx_file_index_mount  ON file_index(mount_id, remote_path);

CREATE TABLE sync_queue (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    inode        INTEGER REFERENCES file_index(inode) ON DELETE CASCADE,
    mount_id     INTEGER NOT NULL REFERENCES mounts(id),
    op           TEXT    NOT NULL CHECK(op IN (
                     'upload','delete','mkdir','rmdir','rename','move'
                 )),
    remote_path  TEXT    NOT NULL,
    remote_dest  TEXT,
    known_etag   TEXT,
    priority     INTEGER NOT NULL DEFAULT 1,
    attempt      INTEGER NOT NULL DEFAULT 0,
    next_attempt INTEGER NOT NULL DEFAULT 0,
    created_at   INTEGER NOT NULL DEFAULT (unixepoch()),
    error_msg    TEXT,
    UNIQUE(inode, op)
);

CREATE INDEX idx_sync_queue_due ON sync_queue(next_attempt, priority DESC);

CREATE TABLE change_tokens (
    mount_id   INTEGER PRIMARY KEY REFERENCES mounts(id),
    token      TEXT    NOT NULL,
    updated_at INTEGER NOT NULL DEFAULT (unixepoch())
);

CREATE TABLE cache_lru (
    inode       INTEGER PRIMARY KEY REFERENCES file_index(inode) ON DELETE CASCADE,
    last_access INTEGER NOT NULL DEFAULT (unixepoch()),
    pinned      INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX idx_cache_lru_access ON cache_lru(last_access) WHERE pinned = 0;

CREATE TABLE xattr_store (
    inode  INTEGER NOT NULL REFERENCES file_index(inode) ON DELETE CASCADE,
    name   TEXT    NOT NULL,
    value  BLOB    NOT NULL,
    PRIMARY KEY (inode, name)
);
