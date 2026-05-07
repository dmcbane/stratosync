//! IPC snapshot types shared between the daemon (producer) and the CLI
//! dashboard (consumer).
//!
//! The daemon listens on a Unix domain socket and serves one JSON-line
//! request/response per connection. Phase 1 supports a single op:
//!
//! ```text
//! → {"op":"status"}\n
//! ← {"ok":true,"data":{...DaemonStatus...}}\n
//! ```
use serde::{Deserialize, Serialize};

/// Top-level dashboard payload. Returned from the daemon in response to
/// `{"op":"status"}`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DaemonStatus {
    pub version:     String,
    pub pid:         u32,
    pub uptime_secs: u64,
    pub mounts:      Vec<MountStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MountStatus {
    pub name:       String,
    pub remote:     String,
    pub mount_path: String,
    pub cache:      CacheStatus,
    pub queue:      QueueStatus,
    pub poller:     PollerStatus,
    pub hydration:  HydrationStatus,
    pub conflicts:  u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct CacheStatus {
    pub used_bytes:   u64,
    pub quota_bytes:  u64,
    pub pinned_count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct QueueStatus {
    pub pending:   u64,
    pub in_flight: Vec<ActiveUpload>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ActiveUpload {
    pub inode:           u64,
    pub path:            String,
    /// Total file size — what we're trying to upload.
    pub size_bytes:      u64,
    /// Unix-epoch seconds when the *current* rclone attempt began.
    /// Resets each retry. The dashboard's "elapsed" column is `now -
    /// started_at_unix`.
    pub started_at_unix: i64,
    /// Unix-epoch seconds when this inode *first* entered the in-flight
    /// state — preserved across retryable errors. Lets the dashboard
    /// distinguish "fresh upload, started 12s ago" from "retry loop,
    /// first attempt was 40 minutes ago." Defaults to `started_at_unix`
    /// for old daemons that don't populate it.
    #[serde(default)]
    pub first_started_unix: i64,
    /// 1 on the initial attempt, 2 on the first retry, etc. Reset to 1
    /// when the inode leaves the queue successfully (or fatally) and
    /// re-enters later. Defaults to 1 for old daemons.
    #[serde(default = "default_attempt")]
    pub attempt:         u32,
    /// Bytes the backend reports as transferred so far for the current
    /// attempt, parsed from `rclone --stats=1s` output. `None` when the
    /// backend doesn't surface progress (mock, webdav). Resets to
    /// `None`/0 each retry.
    #[serde(default)]
    pub bytes_uploaded:  Option<u64>,
}

fn default_attempt() -> u32 { 1 }

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PollerStatus {
    /// "delta" or "full-listing"
    pub mode:                  String,
    pub last_poll_unix:        Option<i64>,
    pub next_poll_unix:        Option<i64>,
    pub consecutive_failures:  u32,
    pub current_interval_secs: u64,
    pub last_error:            Option<String>,
}

impl Default for PollerStatus {
    fn default() -> Self {
        Self {
            mode:                  String::new(),
            last_poll_unix:        None,
            next_poll_unix:        None,
            consecutive_failures:  0,
            current_interval_secs: 0,
            last_error:            None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct HydrationStatus {
    pub active:               u64,
    pub waiters:              u64,
    /// Failed hydrations since the last success. Reset to 0 on any
    /// successful download. The tray flips to a warning icon when this
    /// is non-zero so users learn that downloads are stalling without
    /// having to read journal logs.
    #[serde(default)]
    pub consecutive_failures: u32,
    /// The most recent hydration error message, kept across recoveries
    /// so the dashboard can show "healthy now, last failure was X."
    #[serde(default)]
    pub last_error:           Option<String>,
    /// Unix-epoch seconds of the last failure, also kept across recoveries.
    #[serde(default)]
    pub last_failure_unix:    Option<i64>,
}

/// Wire envelope. The daemon always responds with one of these.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcResponse {
    pub ok:    bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data:  Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl IpcResponse {
    pub fn ok(data: serde_json::Value) -> Self {
        Self { ok: true, data: Some(data), error: None }
    }

    pub fn err(msg: impl Into<String>) -> Self {
        Self { ok: false, data: None, error: Some(msg.into()) }
    }
}

/// Request envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcRequest {
    pub op: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_status() -> DaemonStatus {
        DaemonStatus {
            version: "0.12.0".into(),
            pid: 4242,
            uptime_secs: 3600,
            mounts: vec![MountStatus {
                name:       "gdrive".into(),
                remote:     "gdrive:".into(),
                mount_path: "/home/user/stratosync/Google".into(),
                cache: CacheStatus {
                    used_bytes: 2_100_000_000,
                    quota_bytes: 5_000_000_000,
                    pinned_count: 3,
                },
                queue: QueueStatus {
                    pending: 2,
                    in_flight: vec![ActiveUpload {
                        inode: 123,
                        path: "docs/book.pdf".into(),
                        size_bytes: 4_200_000,
                        started_at_unix: 1_700_000_000,
                        first_started_unix: 1_699_999_700,
                        attempt: 3,
                        bytes_uploaded: Some(1_500_000),
                    }],
                },
                poller: PollerStatus {
                    mode: "full-listing".into(),
                    last_poll_unix: Some(1_700_000_030),
                    next_poll_unix: Some(1_700_000_090),
                    consecutive_failures: 0,
                    current_interval_secs: 60,
                    last_error: None,
                },
                hydration: HydrationStatus {
                    active:  1,
                    waiters: 2,
                    ..Default::default()
                },
                conflicts: 3,
            }],
        }
    }

    #[test]
    fn daemon_status_round_trip() {
        let s = sample_status();
        let json = serde_json::to_string(&s).unwrap();
        let back: DaemonStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn response_ok_serializes_without_error_field() {
        let value = serde_json::to_value(&sample_status()).unwrap();
        let resp = IpcResponse::ok(value);
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"ok\":true"));
        assert!(!json.contains("\"error\""));
    }

    #[test]
    fn response_err_serializes_without_data_field() {
        let resp = IpcResponse::err("unknown op");
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"ok\":false"));
        assert!(json.contains("unknown op"));
        assert!(!json.contains("\"data\""));
    }

    /// An older daemon serializing without the new fields must still
    /// deserialize against the new struct. The IPC socket is local so
    /// daemon and CLI usually move in lockstep, but during a partial
    /// upgrade (new CLI, old daemon still running) the dashboard must
    /// keep working — even if it has to render `attempt=1` and "no
    /// progress info" placeholders.
    #[test]
    fn active_upload_deserializes_legacy_payload() {
        let legacy = r#"{
            "inode": 42,
            "path": "old.txt",
            "size_bytes": 1000,
            "started_at_unix": 1700000000
        }"#;
        let up: ActiveUpload = serde_json::from_str(legacy).unwrap();
        assert_eq!(up.inode, 42);
        assert_eq!(up.attempt, 1, "missing attempt defaults to 1");
        assert_eq!(up.first_started_unix, 0,
            "missing first_started_unix defaults to 0; consumer should fall \
             back to started_at_unix when it sees 0");
        assert_eq!(up.bytes_uploaded, None);
    }
}
