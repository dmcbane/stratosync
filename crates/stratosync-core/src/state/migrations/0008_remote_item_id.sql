-- Migration 0008: stable remote item ID for rename detection.
--
-- Background: OneDrive's delta channel emits Modified-with-new-path for
-- renames *without* a paired Deleted-old-path event. v0.12.x's
-- `upsert_remote_file_gen` matches on `remote_path`, so renames left a
-- stale row at the OLD path while inserting a new row at the NEW path.
-- Reads on the stale row looped EAGAIN forever (v0.12.3 added a self-
-- heal band-aid, but the proper fix is to update the existing row in
-- place when the same item moves).
--
-- This column stores the backend's stable identifier for the file
-- (Microsoft Graph item ID, Google Drive file ID, etc.). When set, the
-- new `upsert_remote_file_by_id_or_path` API matches on it first;
-- only if it's NULL does it fall back to the legacy path-based match.
--
-- Existing rows have NULL until they're touched by a poll that
-- includes the ID (the OneDrive delta provider always sends one;
-- rclone backends include it for providers that expose stable IDs).
-- That's safe: the path-based fallback preserves pre-0.13 behavior
-- for any row whose ID we don't know yet.

ALTER TABLE file_index ADD COLUMN remote_item_id TEXT;

-- Partial unique index — only enforced for rows that have an ID. Rows
-- with NULL item_id are still uniquely keyed by (mount_id, remote_path)
-- via the existing UNIQUE constraint. We deliberately don't try to
-- back-fill IDs in this migration; that's a separate runtime concern
-- (see issue tracker for v0.13 milestone 2).
CREATE UNIQUE INDEX idx_file_index_mount_item_id
    ON file_index(mount_id, remote_item_id)
    WHERE remote_item_id IS NOT NULL;
