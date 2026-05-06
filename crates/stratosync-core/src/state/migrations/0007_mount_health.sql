-- Migration 0007: per-mount hydration health snapshot.
--
-- The daemon writes here when a hydration (download for an open()) fails or
-- recovers. Both the IPC dashboard and the system tray read it to surface a
-- "struggling" indicator without polling the FUSE layer or daemon socket.
--
-- The row is upserted, so each mount has at most one health record. A mount
-- with no failures since startup has either no row or
-- `consecutive_hydration_failures = 0`.

CREATE TABLE mount_health (
    mount_id                       INTEGER PRIMARY KEY REFERENCES mounts(id) ON DELETE CASCADE,
    consecutive_hydration_failures INTEGER NOT NULL DEFAULT 0,
    last_hydration_error           TEXT,
    last_hydration_failure_unix    INTEGER
);
