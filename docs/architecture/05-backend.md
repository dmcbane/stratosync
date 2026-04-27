# Backend Abstraction Layer

## Design Philosophy

The backend layer wraps rclone as an external process. We do **not** link against rclone or call its Go API directly — this keeps stratosync's build simple (no Go toolchain required) and allows rclone version upgrades without recompilation.

The abstraction exposes an async Rust trait so the FUSE layer and sync engine never call rclone directly.

---

## Trait Definition

```rust
#[async_trait]
pub trait Backend: Send + Sync + 'static {
    // Metadata
    async fn stat(&self, path: &str) -> Result<RemoteMetadata>;
    async fn list(&self, path: &str) -> Result<Vec<RemoteMetadata>>;
    async fn list_recursive(&self, path: &str) -> Result<Vec<RemoteMetadata>>;

    // Data transfer
    async fn download(&self, remote: &str, local: &Path) -> Result<()>;
    async fn download_range(
        &self, remote: &str, local: &Path, offset: u64, len: u64,
    ) -> Result<()>;   // default impl returns SyncError::NotSupported
    async fn upload(&self, local: &Path, remote: &str, if_match: Option<&str>)
        -> Result<RemoteMetadata>;

    // Mutations
    async fn mkdir(&self, path: &str) -> Result<()>;
    async fn delete(&self, path: &str) -> Result<()>;
    async fn rename(&self, from: &str, to: &str) -> Result<()>;
    async fn rmdir(&self, path: &str) -> Result<()>;

    // Quota
    async fn about(&self) -> Result<RemoteAbout>;

    // Delta / change detection
    fn supports_delta(&self) -> bool;
    async fn get_start_token(&self) -> Result<String>;          // default: error
    async fn changes_since(&self, token: &str)
        -> Result<(Vec<RemoteChange>, String)>;
}

pub struct RemoteMetadata {
    pub path:      String,
    pub name:      String,
    pub size:      u64,
    pub mtime:     SystemTime,
    pub is_dir:    bool,
    pub etag:      Option<String>,
    pub checksum:  Option<String>,   // SHA1/MD5 where available
    pub mime_type: Option<String>,
}
```

---

## `RcloneBackend` Implementation

The concrete implementation drives rclone via subprocess.

### Backend variants

Two concrete `Backend` implementations are shipped:

| Implementation | Use | How |
|----------------|-----|-----|
| **`RcloneBackend`** (default) | All operations | `tokio::process::Command` per call |
| **`WebDavSidecarBackend`** (opt-in) | All operations | One long-running `rclone serve webdav` per mount, plain HTTP/WebDAV via `reqwest` |

Set `[daemon] webdav_sidecar = true` to enable the sidecar. The daemon spawns
one `rclone serve webdav` subprocess per mount on a port computed from the
mount ID (so multiple mounts don't collide), and routes all `Backend` calls
through HTTP/WebDAV verbs (GET, PUT, PROPFIND, MKCOL, DELETE, MOVE).

The sidecar eliminates per-operation rclone subprocess startup (~30 ms cold
start) and gives clean HTTP `Range:` support for `download_range`. Trade-off:
one extra long-running process per mount, plus the rclone `serve webdav`
implementation's quirks. RcloneBackend remains the conservative default.

The `Backend` trait abstracts over both — every consumer (FUSE layer, sync
engine, CLI) sees the same async interface. Selecting the variant is a
daemon-level concern.

### Delta providers

Delta-API support is layered on top of `RcloneBackend` via the
`DeltaProvider` trait. `init_delta()` is called once at startup and
constructs a provider based on the rclone remote type:

| `type =` | DeltaProvider |
|----------|---------------|
| `drive` | `GoogleDriveDelta` (`pageToken` against the Changes API) |
| `onedrive` | `OneDriveDelta` (`/delta` endpoint, full-path responses) |
| anything else | none — `supports_delta()` returns `false` |

Both providers handle their own OAuth refresh: GoogleDriveDelta force-refreshes
via `rclone about` + `rclone config show` when it sees HTTP 401;
OneDriveDelta hits Microsoft's token endpoint directly.

GDrive shared drives are not yet supported — see [ROADMAP.md](../ROADMAP.md).

### rclone invocation patterns

```rust
// List directory as JSON
rclone lsjson --no-modtime "gdrive:Documents" 

// Stat a single file
rclone lsjson --no-modtime "gdrive:Documents/report.pdf" --include "report.pdf"

// Download
rclone copy "gdrive:Documents/report.pdf" "/home/dale/.cache/stratosync/gdrive/Documents/"

// Upload with checksum check
rclone copy "/home/dale/.cache/stratosync/gdrive/Documents/report.pdf" "gdrive:Documents/" --checksum

// Delete
rclone delete "gdrive:Documents/old.pdf"

// Move/rename
rclone moveto "gdrive:Documents/old.pdf" "gdrive:Documents/new.pdf"

// Space usage  
rclone about "gdrive:" --json
```

### JSON parsing

`rclone lsjson` output:

```json
[
  {
    "Path": "Documents/report.pdf",
    "Name": "report.pdf",
    "Size": 102400,
    "MimeType": "application/pdf",
    "ModTime": "2025-03-15T10:00:00.000000000Z",
    "IsDir": false,
    "Hashes": { "SHA-1": "abc123...", "MD5": "def456..." },
    "ID": "1BxiMVs0XRA5nFMdKvBdBZjgmUUqptlbs74OgVE2upms"
  }
]
```

We deserialize this with `serde_json` directly into `RemoteMetadata`.

### Error handling

rclone exit codes:
- `0` = success
- `1` = syntax/usage error
- `2` = error not otherwise categorized  
- `3` = directory not found
- `4` = file not found
- `5` = temporary error (retry)
- `6` = less serious errors
- `7` = fatal error
- `8` = transfer exceeded (quota)

We map these to our `BackendError` enum:

```rust
pub enum BackendError {
    NotFound,
    PermissionDenied,
    Conflict(String),    // ETag mismatch
    QuotaExceeded,
    NetworkError(String),
    Transient(String),   // retry eligible
    Fatal(String),
}
```

### ETag / conflict detection

rclone doesn't expose an `if-match` upload directly. We simulate it:

```
1. Fetch remote etag via lsjson before upload
2. Upload the file
3. Fetch remote etag again after upload
4. If post-upload etag doesn't match what we uploaded (concurrent write):
   → treat as conflict
```

This is a TOCTOU window, but it's the same approach used by rclone itself and is acceptable for a sync daemon (not a transactional database).

For backends that support it (WebDAV, Nextcloud), we can pass `--header "If-Match: <etag>"` directly via `rclone copyto --header`.

---

## Configuration per backend

```toml
[mount.gdrive]
remote = "gdrive:/"
mount_path = "~/GoogleDrive"
cache_quota = "10 GiB"
poll_interval = "30s"

[mount.gdrive.rclone]
# Extra rclone flags for this remote
extra_flags = ["--drive-acknowledge-abuse"]
bwlimit = "10M"          # rate limit uploads to 10 MB/s
transfers = 4            # max concurrent transfers
checkers = 8             # max concurrent stat checks
```

---

---

## Testing

The `Backend` trait enables full unit testing of the sync engine and FUSE layer against a mock:

```rust
pub struct MockBackend {
    files: Arc<Mutex<HashMap<String, RemoteMetadata>>>,
    contents: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    fail_on: Arc<Mutex<HashSet<String>>>,  // paths that should return errors
}
```

Integration tests run against a real rclone configured with a local `local:` or `memory:` backend (no cloud credentials needed).
