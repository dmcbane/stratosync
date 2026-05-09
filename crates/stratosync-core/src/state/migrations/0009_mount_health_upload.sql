-- Migration 0009: extend mount_health with upload-side counters.
--
-- 0007 added the hydration columns. We later realized the same persistent
-- "consecutive failures since last success" signal is just as valuable for
-- uploads: a 41-attempt retry loop is invisible once the file falls out of
-- the in-flight panel, and the tray was only flipping its warning icon on
-- hydration failures. Adding upload analogues so the tray, dashboard, and
-- Prometheus all surface upload health on the same footing.
--
-- Defaults match the hydration columns: zero on rows that don't have any
-- failures recorded yet.

ALTER TABLE mount_health ADD COLUMN consecutive_upload_failures INTEGER NOT NULL DEFAULT 0;
ALTER TABLE mount_health ADD COLUMN last_upload_error           TEXT;
ALTER TABLE mount_health ADD COLUMN last_upload_failure_unix    INTEGER;
