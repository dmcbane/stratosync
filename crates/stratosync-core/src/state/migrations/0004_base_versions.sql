-- Base version object store for 3-way merge conflict resolution.
-- Maps each (inode, mount_id) to the SHA-256 hash of its last known-good
-- content, stored as a content-addressed blob on disk.

CREATE TABLE base_versions (
    inode       INTEGER NOT NULL,
    mount_id    INTEGER NOT NULL REFERENCES mounts(id),
    object_hash TEXT    NOT NULL,
    file_size   INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL DEFAULT (unixepoch()),
    PRIMARY KEY (inode, mount_id),
    FOREIGN KEY (inode) REFERENCES file_index(inode) ON DELETE CASCADE
);

CREATE INDEX idx_base_versions_hash ON base_versions(object_hash);
CREATE INDEX idx_base_versions_age  ON base_versions(updated_at);
