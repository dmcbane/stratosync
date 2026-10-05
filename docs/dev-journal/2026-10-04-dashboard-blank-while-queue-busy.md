# 2026-10-04 — dashboard blank and unkillable while the upload queue is busy

## Symptom
During a heavy read of the OneDrive mount, `stratosync dashboard`
showed an empty screen and ignored `q`; only killing it worked. Run
again later, the same command answered in ~100 ms.

## Root cause
Two waits stacked with no bound:
1. `tui_loop` awaited `fetch_status` *before* its first `draw` and
   before polling keys, with no timeout.
2. The daemon's `status` handler awaits `UploadQueue::snapshot()`, which
   is a message on the queue's own trigger channel. The queue loop
   handles `Conflict` results inline — `conflict::resolve` downloads the
   remote file to compare bytes — so a snapshot waits behind it, and
   behind any triggers already in the channel.

With spurious conflicts on every hydrated file (see
`2026-10-04-hydration-triggers-reupload.md`) and OneDrive throttling,
the loop was busy for minutes.

## Fix
- Dashboard: background fetch task with a 3 s timeout; draw and handle
  keys on every tick regardless.
- Daemon: `queue_snapshot_or_busy` caps the snapshot at 1 s and returns
  `QueueStatus { busy: true, .. }`; clients show "busy", not zeros.

## Follow-up
Conflict resolution still runs inline in the queue loop, so uploads
stall while one resolves. Moving it off the loop needs a per-inode
guard against starting a new upload of the same file mid-resolve.
