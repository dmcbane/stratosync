-- Migration 0011: clear stale upload-failure health from the pre-
-- beta.13 "dirty but no cache_path" bug.
--
-- Background: migration 0010 cleaned the poisoned `file_index` rows
-- but left `mount_health.consecutive_upload_failures` alone — those
-- counters had been incremented once per fatal retry, and only
-- `record_upload_success` decrements them. After migration 0010 the
-- failures are no longer real (the rows that produced them are
-- gone), but the dashboard halts the mount with a "554 consecutive
-- failures — backend fatal: inode N: dirty but no cache_path"
-- banner that never clears.
--
-- Migration 0010's content is now immutable (already applied to
-- users on beta.13), so this targeted retroactive clean lives in a
-- separate migration. Three populations covered:
--   1. Users who hit the bug under beta.11/12 and upgraded straight
--      to beta.13. (0010 cleaned their rows; counter still stuck.)
--   2. Users on beta.13 right now whose dashboard is halted.
--      (Their 0010 ran already; this is their fix.)
--   3. Users who upgraded beta.11 → beta.12 → beta.13 where the
--      beta.12 runtime sweep ate the rows before 0010 ever saw
--      them. (Same shape, same fix.)
--
-- Pattern-match on the exact bug string so legitimate transient
-- upload errors (rclone timeout, auth failures, etc.) keep their
-- counters intact. The exact phrase only ever came from
-- `upload_queue.rs::run_upload`'s `Fatal` branch in pre-beta.13,
-- which is unreachable post-fix.
UPDATE mount_health
SET consecutive_upload_failures = 0,
    last_upload_error           = NULL,
    last_upload_failure_unix    = NULL
WHERE last_upload_error LIKE '%dirty but no cache_path%';
