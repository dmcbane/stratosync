/// Central domain types shared across all stratosync crates.
use std::time::SystemTime;
use serde::{Deserialize, Serialize};

// ── Inode number ──────────────────────────────────────────────────────────────

/// Inode number. Allocated from SQLite's auto-increment; never reused within a
/// DB lifetime. Inode 1 is always the root of a mount.
pub type Inode = u64;
pub const FUSE_ROOT_INODE: Inode = 1;

// ── File kind ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileKind {
    File,
    Directory,
    Symlink,
}

// ── Hydration / sync status ───────────────────────────────────────────────────

/// The lifecycle state of a file entry in the daemon.
/// See docs/architecture/03-sync-engine.md for the full state machine diagram.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SyncStatus {
    /// Metadata known; no local data. Will hydrate on open().
    Remote,
    /// Download in progress. FUSE open() blocks until this resolves.
    Hydrating,
    /// Local cache is fresh and matches remote (by ETag).
    Cached,
    /// Local cache has unsaved writes; upload pending.
    Dirty,
    /// Upload in progress.
    Uploading,
    /// Remote changed while we had a cached copy; re-hydrate on next open().
    Stale,
    /// Both local and remote changed concurrently. Conflict file created.
    Conflict,
}

impl SyncStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Remote    => "remote",
            Self::Hydrating => "hydrating",
            Self::Cached    => "cached",
            Self::Dirty     => "dirty",
            Self::Uploading => "uploading",
            Self::Stale     => "stale",
            Self::Conflict  => "conflict",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "remote"    => Some(Self::Remote),
            "hydrating" => Some(Self::Hydrating),
            "cached"    => Some(Self::Cached),
            "dirty"     => Some(Self::Dirty),
            "uploading" => Some(Self::Uploading),
            "stale"     => Some(Self::Stale),
            "conflict"  => Some(Self::Conflict),
            _           => None,
        }
    }

    /// Returns true if the file has local data that must not be evicted.
    pub fn has_local_data(self) -> bool {
        matches!(self, Self::Cached | Self::Dirty | Self::Uploading | Self::Conflict)
    }

    /// Returns true if a hydration is already in progress or queued.
    pub fn is_hydrating(self) -> bool {
        self == Self::Hydrating
    }

    /// Returns true if the file needs to be fetched before it can be read.
    pub fn needs_hydration(self) -> bool {
        matches!(self, Self::Remote | Self::Stale)
    }
}

// ── File entry (the core record in file_index) ────────────────────────────────

#[derive(Debug, Clone)]
pub struct FileEntry {
    pub inode:       Inode,
    pub mount_id:    u32,
    pub parent:      Inode,
    pub name:        String,
    pub remote_path: String,
    pub kind:        FileKind,
    pub size:        u64,
    pub mtime:       SystemTime,
    pub etag:        Option<String>,
    pub status:      SyncStatus,
    pub cache_path:  Option<std::path::PathBuf>,
    pub cache_size:  Option<u64>,
    pub dir_listed:  Option<SystemTime>,
}

impl FileEntry {
    /// Returns the file extension (without leading dot), if any.
    pub fn extension(&self) -> Option<&str> {
        std::path::Path::new(&self.name)
            .extension()
            .and_then(|e| e.to_str())
    }

    /// Returns the file stem (name without extension).
    pub fn stem(&self) -> &str {
        std::path::Path::new(&self.name)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(&self.name)
    }
}

// ── Remote metadata (what rclone returns from lsjson) ─────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct RcloneLsJsonEntry {
    pub path:      String,
    pub name:      String,
    pub size:      i64,
    #[serde(rename = "MimeType")]
    pub mime_type: Option<String>,
    pub mod_time:  String,   // RFC3339
    pub is_dir:    bool,
    pub hashes:    Option<std::collections::HashMap<String, String>>,
    #[serde(rename = "ID")]
    pub id:        Option<String>,
}

#[derive(Debug, Clone)]
pub struct RemoteMetadata {
    pub path:      String,
    pub name:      String,
    pub size:      u64,
    pub mtime:     SystemTime,
    pub is_dir:    bool,
    pub etag:      Option<String>,
    pub checksum:  Option<String>,
    pub mime_type: Option<String>,
}

impl TryFrom<RcloneLsJsonEntry> for RemoteMetadata {
    type Error = anyhow::Error;

    fn try_from(e: RcloneLsJsonEntry) -> Result<Self, Self::Error> {
        let mtime = chrono::DateTime::parse_from_rfc3339(&e.mod_time)
            .map(|dt| SystemTime::from(dt))
            .unwrap_or(SystemTime::UNIX_EPOCH);

        // Prefer SHA-1 or MD5 as a stable content identifier; fall back to
        // provider-specific ID (Google Drive file ID, etc.) as a poor-man's
        // etag.
        let etag = e.hashes.as_ref()
            .and_then(|h| h.get("SHA-1").or_else(|| h.get("MD5")).cloned())
            .or_else(|| e.id.clone());

        let checksum = e.hashes.as_ref()
            .and_then(|h| h.get("SHA-256").cloned());

        Ok(Self {
            path:      e.path,
            name:      e.name,
            size:      e.size.max(0) as u64,
            mtime,
            is_dir:    e.is_dir,
            etag,
            checksum,
            mime_type: e.mime_type,
        })
    }
}

// ── Remote change (detected by poller) ───────────────────────────────────────

#[derive(Debug, Clone)]
pub enum RemoteChange {
    Added    { meta: RemoteMetadata },
    Modified { meta: RemoteMetadata, old_etag: Option<String> },
    Deleted  { path: String },
}

// ── Sync queue operations ─────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SyncOp {
    Upload,
    Delete,
    Mkdir,
    Rmdir,
    Rename,
    Move,
}

impl SyncOp {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Upload => "upload",
            Self::Delete => "delete",
            Self::Mkdir  => "mkdir",
            Self::Rmdir  => "rmdir",
            Self::Rename => "rename",
            Self::Move   => "move",
        }
    }
}

// ── Errors ────────────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error("not found: {0}")]
    NotFound(String),

    #[error("permission denied: {0}")]
    PermissionDenied(String),

    #[error("etag conflict: local={local:?} remote={remote:?}")]
    Conflict { local: Option<String>, remote: Option<String> },

    #[error("quota exceeded")]
    QuotaExceeded,

    #[error("network error: {0}")]
    Network(String),

    #[error("transient error (will retry): {0}")]
    Transient(String),

    #[error("backend fatal: {0}")]
    Fatal(String),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl SyncError {
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Network(_) | Self::Transient(_))
    }
}
