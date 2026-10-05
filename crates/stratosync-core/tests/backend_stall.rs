//! Tests for the rclone transfer stall watchdog.
//!
//! Background: `RcloneBackend` used to wrap every rclone invocation —
//! including multi-gigabyte transfers — in a single fixed wall-clock
//! timeout (120 s). Any upload/download that couldn't finish in that
//! window was killed mid-stream, mapped to a retryable `Network` error,
//! and restarted from byte zero forever. A real 2.4 GB upload to Google
//! Drive failed 613 times in a row this way.
//!
//! The fix replaces the fixed wall-clock with a *stall* watchdog: a
//! transfer is only aborted when it makes no progress (no rclone stats
//! output) for `stall_timeout`. An actively-progressing transfer runs to
//! completion regardless of total size.
//!
//! These tests substitute a fake `rclone` shell script (via the explicit
//! `RcloneBackend::with_binary` constructor) so they exercise the real
//! `run_with_progress` path with no cloud, no FUSE, and no network.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use stratosync_core::{Backend, RcloneBackend, SyncError};

/// Write `body` as an executable fake-rclone script and return its path.
fn write_fake_rclone(dir: &Path, body: &str) -> PathBuf {
    let p = dir.join("fake-rclone");
    std::fs::write(&p, body).unwrap();
    let mut perms = std::fs::metadata(&p).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&p, perms).unwrap();
    p
}

/// Run a download through the backend with a 1 s stall timeout, against
/// the given fake-rclone body. Returns the `download_with_progress`
/// result.
async fn run_download(script_body: &str) -> Result<(), SyncError> {
    let tmp = tempfile::tempdir().unwrap();
    let script = write_fake_rclone(tmp.path(), script_body);

    let be = RcloneBackend::with_binary("fake:/", &script)
        .with_stall_timeout(Duration::from_secs(1));

    let local = tmp.path().join("out.bin");
    let (tx, _rx) = tokio::sync::mpsc::channel::<u64>(8);
    let res = be.download_with_progress("remote/file", &local, tx).await;
    // Keep tmp alive until the call returns (script must remain on disk).
    drop(tmp);
    res
}

/// Regression: a transfer that keeps making progress for ~3 s — far
/// longer than the 1 s stall window — must SUCCEED. Under the old fixed
/// 1 s timeout this would be killed at 1 s; the stall watchdog resets the
/// deadline on every progress line, so it runs to completion.
#[tokio::test]
async fn progressing_transfer_outlives_stall_window() {
    // 15 stats lines, 0.2 s apart on stderr = ~3 s of steady progress.
    let body = r#"#!/usr/bin/env bash
for i in $(seq 1 15); do
  printf '{"level":"error","msg":"x","stats":{"bytes":%d,"totalBytes":15000}}\n' "$((i*1000))" >&2
  sleep 0.2
done
exit 0
"#;
    let res = run_download(body).await;
    assert!(res.is_ok(), "progressing transfer should succeed, got {res:?}");
}

/// A genuinely wedged transfer — one line then silence — must still be
/// aborted, mapped to a retryable `Network` error. (Never swallow: it
/// must surface as an error, not a false success.)
#[tokio::test]
async fn stalled_transfer_is_aborted() {
    let body = r#"#!/usr/bin/env bash
printf '{"level":"error","msg":"x","stats":{"bytes":0,"totalBytes":10000}}\n' >&2
sleep 5
exit 0
"#;
    let res = run_download(body).await;
    match res {
        Err(SyncError::Network(msg)) => {
            assert!(msg.contains("stall"), "expected a stall message, got {msg:?}");
        }
        other => panic!("expected Network(stall) error, got {other:?}"),
    }
}

/// rclone exit-code → `SyncError` mapping must be unchanged by the
/// watchdog: a fast non-zero exit (code 3 = directory not found) still
/// maps to `NotFound`, not a stall/timeout.
#[tokio::test]
async fn exit_code_mapping_preserved() {
    let body = r#"#!/usr/bin/env bash
echo "directory not found" >&2
exit 3
"#;
    let res = run_download(body).await;
    assert!(
        matches!(res, Err(SyncError::NotFound(_))),
        "exit 3 should map to NotFound, got {res:?}",
    );
}

/// Run `download_range` with a 1 s *wall-clock* timeout and a 1 s stall
/// window against the given fake-rclone body.
async fn run_range(script_body: &str) -> Result<Vec<u8>, SyncError> {
    let tmp = tempfile::tempdir().unwrap();
    let script = write_fake_rclone(tmp.path(), script_body);
    let mut be = RcloneBackend::with_binary("fake:/", &script)
        .with_stall_timeout(Duration::from_secs(1));
    be.timeout = Duration::from_secs(1);
    let res = be.download_range("remote/file", 0, 4).await;
    drop(tmp);
    res
}

/// Live failure: `rclone cat --offset/--count` (the range read FUSE uses
/// to serve the first bytes of an unhydrated file) ran under the fixed
/// metadata timeout. On a throttled OneDrive a 3 MB file's range read hit
/// it four times in a row ("range download failed … rclone timed out"),
/// stalling the reader. Range reads are transfers: a slow-but-alive one
/// must complete, and the bytes on stdout must come back intact.
#[tokio::test]
async fn progressing_range_download_outlives_wall_clock_timeout() {
    let body = r#"#!/usr/bin/env bash
for i in $(seq 1 15); do
  printf '{"level":"error","msg":"x","stats":{"bytes":%d,"totalBytes":4}}\n' "$i" >&2
  sleep 0.2
done
printf 'DATA'
exit 0
"#;
    let res = run_range(body).await;
    assert_eq!(res.ok().as_deref(), Some(&b"DATA"[..]),
        "a progressing range read must not be killed by the wall-clock timeout");
}

#[tokio::test]
async fn stalled_range_download_is_aborted() {
    let body = r#"#!/usr/bin/env bash
printf '{"level":"error","msg":"x","stats":{"bytes":0,"totalBytes":4}}\n' >&2
sleep 5
exit 0
"#;
    match run_range(body).await {
        Err(SyncError::Network(msg)) => assert!(msg.contains("stall"), "{msg:?}"),
        other => panic!("expected Network(stall), got {other:?}"),
    }
}

/// With `--stats=1s`, the last JSON log line before rclone exits is
/// usually a stats line. The error message must come from the real error
/// line — not surface as "read: not found: 0 B / 0 B, -, 0 B/s, ETA -".
#[tokio::test]
async fn error_message_ignores_trailing_stats_lines() {
    let body = r#"#!/usr/bin/env bash
printf '{"level":"error","msg":"error listing: directory not found"}\n' >&2
printf '{"level":"error","msg":"0 B / 0 B, -, 0 B/s, ETA -","stats":{"bytes":0,"totalBytes":0}}\n' >&2
exit 3
"#;
    match run_download(body).await {
        Err(SyncError::NotFound(msg)) => assert_eq!(msg, "error listing: directory not found"),
        other => panic!("expected NotFound with the real message, got {other:?}"),
    }
}
