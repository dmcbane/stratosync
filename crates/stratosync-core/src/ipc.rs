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
    pub size_bytes:      u64,
    pub started_at_unix: i64,
}

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
    pub active:  u64,
    pub waiters: u64,
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
                hydration: HydrationStatus { active: 1, waiters: 2 },
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
}
