# Fixed 120 s rclone timeout killed large uploads forever

## Symptom

A 2.4 GB archive (`restoration_mediamtxpi.img.xz`) on the gdrive mount
failed to upload **613 consecutive times over ~33 hours**. Every attempt
logged the identical line:

```
upload transient error: network error: rclone timed out — will retry … attempt=N
```

`mount_health.consecutive_upload_failures = 613`,
`last_upload_error = "network error: rclone timed out"`. The file row sat
in `status = uploading` with a complete cache file and an empty
`remote_item_id` (it never reached the remote). Uploading the *same* file
through the Google Drive web UI succeeded on the first try.

## Root cause

`RcloneBackend` hardcoded a single 120-second wall-clock timeout on
**every** rclone invocation — `timeout: Duration::from_secs(120)` —
applied uniformly by both runner methods (`run` and `run_with_progress`)
via `tokio::time::timeout(self.timeout, …)`. That value gated a fast
`stat` and a multi-gigabyte transfer the same way.

A 2.4 GB upload cannot finish in 120 s on a normal home upstream link, so
`child.wait()` was cut off at 120 s and mapped to
`SyncError::Network("rclone timed out")`. That error `is_retryable()`, so
the upload queue re-queued it and **restarted from byte zero** every time
— it could never converge. The tell: `"rclone timed out"` is the
*daemon's own* string (raised on the `tokio::time::timeout` Err branch),
not anything rclone emits — proof the daemon's deadline fired, not a real
network fault. The browser worked because it uses resumable chunked
upload with no equivalent wall-clock cap.

## Fix

A fixed wall-clock is the wrong tool for data transfers. `run_with_progress`
(the path used by `upload_with_progress` / `download_with_progress`) now
uses a **stall watchdog**: the stderr task already reads rclone's
`--stats=1s` output line-by-line, so it bumps a shared `AtomicU64`
liveness counter on every line. The wait loop races `child.wait()`
against `sleep(stall_timeout)`; if the counter hasn't advanced across a
full stall window, the transfer is wedged → abort with
`Network("rclone stalled (no progress for Ns)")`. As long as bytes keep
moving, the deadline keeps resetting, so size/total-time no longer
matter. `stall_timeout` defaults to 120 s and is overridable via
`[mount.rclone] stall_timeout_secs`. The old fixed `timeout` is retained
for quick metadata ops (`stat`, `lsjson`, `mkdir`, …). Transfers also now
pass rclone's own `--timeout 120s --contimeout 60s` as a backstop.

While here: `[mount.rclone]` (`extra_flags`/`bwlimit`/`transfers`/
`checkers`) was defined in config but never passed to the backend — wired
in via `RcloneBackend::with_rclone_config`.

## Follow-up

Tests substitute a fake rclone via the new `RcloneBackend::with_binary`
constructor (no env-var race) — see `tests/backend_stall.rs`. To recover
an already-wedged file after upgrading: restart the daemon and
`stratosync push <path>` to force it back through the queue; success
resets `consecutive_upload_failures` via `record_upload_success`.
