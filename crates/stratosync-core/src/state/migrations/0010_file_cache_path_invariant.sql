-- Migration 0010: enforce the file-row cache_path invariant.
--
-- Background: a file row whose status is one of `cached`, `dirty`, or
-- `uploading` claims to have on-disk content the cloud doesn't yet
-- have (or has an out-of-date copy of). The upload queue relies on
-- `cache_path` to find that content; a NULL cache_path in those
-- statuses is a logically impossible state.
--
-- Two pre-v0.13.0-beta.12 paths reached the impossible state:
--   * `setattr(size=N)` on a never-hydrated file materialized a cache
--     file at a synthesized path but recorded `cache_path = NULL`
--     (fuse/mod.rs).
--   * `versions::restore` on a never-hydrated file did the same shape
--     (cli/commands/versions.rs).
--
-- Both leaked rows like `(status='dirty', cache_path=NULL)` that
-- looped in the upload queue forever, fatal-erroring once per cycle
-- and firing a desktop notification each time. The user-facing
-- symptom was a 277-row notification storm on every daemon restart.
--
-- Statuses NOT covered by the invariant:
--   * `remote` / `hydrating` — by definition no local content yet.
--   * `stale` — the poller may flip a never-hydrated row from
--     `remote` to `stale` on a later visit (the upsert CASE in
--     `upsert_remote_file_*` does this); a stale row with NULL
--     cache_path means "remote changed before we ever downloaded
--     it." Legitimate.
--   * `conflict` — the conflict file's row may track a remote-only
--     `.conflict.{ts}.{hash}` sibling that was never pinned locally
--     (sync/conflict.rs:255). Legitimate.

-- Step 1: clean any existing poisoned rows before installing the
-- guard. Reverting to `remote` is correct: the cloud version was
-- never overwritten (no successful upload happened), so the cloud
-- copy is the source of truth and a future open() will re-hydrate
-- from it. Belt-and-suspenders alongside the runtime
-- `reset_stuck_dirty_files_without_cache_path` sweep added in
-- daemon startup — if that sweep ever stops running (renamed,
-- short-circuited, etc.) this still clears the slate.
UPDATE file_index
SET status = 'remote'
WHERE kind = 'file'
  AND status IN ('cached','dirty','uploading')
  AND cache_path IS NULL;

-- Step 2: enforce the invariant going forward. SQLite doesn't
-- support `ALTER TABLE ADD CHECK CONSTRAINT`, so we use BEFORE
-- INSERT/UPDATE triggers that RAISE(ABORT). The error message
-- names the invariant explicitly so a future developer who hits
-- it in a test sees the connection rather than just `SQL error`.
CREATE TRIGGER file_index_dirty_requires_cache_path_insert
BEFORE INSERT ON file_index
WHEN NEW.kind = 'file'
  AND NEW.status IN ('cached','dirty','uploading')
  AND NEW.cache_path IS NULL
BEGIN
  SELECT RAISE(ABORT,
    'file_index invariant: status in (cached,dirty,uploading) requires cache_path');
END;

CREATE TRIGGER file_index_dirty_requires_cache_path_update
BEFORE UPDATE ON file_index
WHEN NEW.kind = 'file'
  AND NEW.status IN ('cached','dirty','uploading')
  AND NEW.cache_path IS NULL
BEGIN
  SELECT RAISE(ABORT,
    'file_index invariant: status in (cached,dirty,uploading) requires cache_path');
END;
