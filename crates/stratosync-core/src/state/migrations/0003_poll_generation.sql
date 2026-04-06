-- Migration 0003: Poll generation counter
-- Tracks which poll cycle last confirmed each file's existence on the remote.
-- After a poll, entries with an old generation are deletion candidates.

ALTER TABLE file_index ADD COLUMN poll_generation INTEGER NOT NULL DEFAULT 0;
CREATE INDEX idx_file_index_generation ON file_index(mount_id, poll_generation);
