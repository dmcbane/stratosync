# Hydration health visibility (`mount_health` table)

**Date:** 2026-05-06
**Version:** v0.12.2

## Symptom

A user copying a large remote file to local saw the operation stall for
minutes. The daemon was retrying the download — journalctl showed a
loud spew of:

```
range download failed, waiting for full hydration: not found
read: transient error (will retry): hydration failed: 5
read: not found: Attempt 3/3 failed with 1 errors and: directory not found
```

— but nothing in the tray, dashboard, or any other user-facing surface
indicated that downloads were unhealthy. The user had to know to run
`journalctl --user -u stratosyncd -f` to see anything was wrong.

## Root cause

There was no plumbing for read/hydration failures. We surfaced *upload*
failures via `notify-send` (sync/upload_queue.rs) and *poller* failures
in `PollerStatus` (which the dashboard renders), but the FUSE hydration
path only logged. The tray and dashboard had no way to learn about a
stalled download short of polling FUSE state directly.

The underlying "ino=… not found" failure is a separate bug — likely a
delta-poller rename racing an in-flight download — and is not addressed
here. v0.12.2 is purely a visibility fix so users learn that something
is wrong.

## Fix (v0.12.2)

Added migration `0007_mount_health.sql` with a tiny per-mount table:

```sql
CREATE TABLE mount_health (
    mount_id                       INTEGER PRIMARY KEY REFERENCES mounts(id) ON DELETE CASCADE,
    consecutive_hydration_failures INTEGER NOT NULL DEFAULT 0,
    last_hydration_error           TEXT,
    last_hydration_failure_unix    INTEGER
);
```

`do_hydrate` (fuse/mod.rs) now calls `record_hydration_failure(mid, err)`
on its rollback branch and `record_hydration_success(mid)` on the Ok
branch. Both go through the same DB connection the FUSE layer already
holds, so there's no new `Arc` plumbing. Failures from the range-download
fallback path are not double-counted because that path falls through to
`hydrate_if_needed → do_hydrate`, which is the single recording site.

The IPC `HydrationStatus` gained `consecutive_failures`, `last_error`,
and `last_failure_unix` fields (with `#[serde(default)]` so older clients
keep working). `DaemonState::collect_mount_status` reads them from the
DB. The tray polls the same DB rows directly — no daemon-IPC dependency
added to the tray.

## Two non-obvious design choices

### Why "success keeps `last_hydration_error`"

`record_hydration_success` zeros the counter but does **not** clear
`last_hydration_error` / `last_hydration_failure_unix`. The dashboard can
then show "healthy now, last failure was 12 minutes ago", which is more
useful than a binary green/red. A user who only checks the tray after
hearing about a problem still gets actionable diagnostics.

### Why the tray threshold is 3, not 1

The tray flips icons at `HYDRATION_DEGRADED_THRESHOLD = 3` consecutive
failures. A single transient error (e.g. one HTTP 503 from the backend)
is common on flaky networks and would just cause icon flicker. Three in
a row is the signal that "this isn't recovering on its own." The
dashboard, by contrast, shows the count starting at 1 — it's an
operator surface, not an at-a-glance status indicator.

## Why no `notify-send` popup yet

Was tempted, but reads on a stuck inode fire ~10×/sec — without per-inode
rate-limiting a `notify-send` from the failure site would carpet-bomb
the user's notification daemon. A future change can add a popup gated on
`consecutive_failures >= N` so it only fires once when things actually
get stuck, then again on recovery.

## Tests

- `state::tests::mount_health_*` (4 tests, core) — DB-level upsert,
  reset, and "success on clean mount" no-op.
- `fuse::hydrate_tests::do_hydrate_*` (2 tests, daemon) — end-to-end
  with `MockBackend`: success records recovery, `fail_on(path)`
  increments the counter and stashes the error message.
- `tray::tests::*` (6 tests, tray) — icon/tooltip decision matrix
  including conflict precedence and multi-mount roll-up.
