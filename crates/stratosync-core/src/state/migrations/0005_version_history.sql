-- File version history. Records snapshots of file content at the moments
-- when content was about to be overwritten — either by a poller-detected
-- remote change replacing a cached file, or by a successful upload of a
-- new local version.
--
-- Each row references a content-addressed blob in `.bases/objects/`
-- (same store as `base_versions`). A blob is unreferenced — and thus
-- safe to delete from disk — only when no row in `version_history` AND
-- no row in `base_versions` points at it.

CREATE TABLE version_history (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    inode       INTEGER NOT NULL,
    mount_id    INTEGER NOT NULL REFERENCES mounts(id),
    object_hash TEXT    NOT NULL,
    recorded_at INTEGER NOT NULL DEFAULT (unixepoch()),
    file_size   INTEGER NOT NULL,
    etag        TEXT,
    -- 'before_poll'   — local cache content captured before the poller
    --                   replaced it with a remote change.
    -- 'after_upload'  — content of the file we just uploaded.
    -- 'manual'        — reserved for future use (`stratosync snapshot`).
    source      TEXT    NOT NULL CHECK (source IN ('before_poll','after_upload','manual')),
    FOREIGN KEY (inode) REFERENCES file_index(inode) ON DELETE CASCADE
);

CREATE INDEX idx_version_history_inode ON version_history(inode, recorded_at DESC);
CREATE INDEX idx_version_history_hash  ON version_history(object_hash);
