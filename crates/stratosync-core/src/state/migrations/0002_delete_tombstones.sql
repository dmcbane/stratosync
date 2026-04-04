-- Migration 0002: Delete tombstones
-- Prevents the poller from re-adding files that were just deleted locally
-- but haven't been removed from the remote yet.

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
