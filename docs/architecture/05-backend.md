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
    async fn download_range(&self, remote: &str, local: &Path, offset: u64, len: u64) -> Result<()>;
    async fn upload(&self, local: &Path, remote: &str, if_match: Option<&str>) -> Result<RemoteMetadata>;

    // Mutations
    async fn mkdir(&self, path: &str) -> Result<()>;
    async fn delete(&self, path: &str) -> Result<()>;
    async fn rename(&self, from: &str, to: &str) -> Result<()>;

    // Quota
    async fn about(&self) -> Result<RemoteAbout>;

    // Delta / change detection
    async fn changes_since(&self, token: &str) -> Result<(Vec<RemoteChange>, String)>;
    fn supports_delta(&self) -> bool;
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

### Process lifecycle

We maintain **two rclone modes**:

| Mode | Use | How |
|------|-----|-----|
| **Command mode** | One-shot ops: stat, list, mkdir, delete, rename | `tokio::process::Command` per call |
| **Serve mode** | Bulk transfers (download/upload) | Long-running `rclone serve webdav` or `rclone rcat` |

For the MVP, we use command mode exclusively. Serve mode is a Phase 2 optimization that reduces per-transfer startup overhead from ~30ms to ~1ms.

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

## Phase 2: rclone serve mode

For high-throughput scenarios, replace command-per-operation with a long-running rclone WebDAV server:

```bash
rclone serve webdav "gdrive:/" --addr 127.0.0.1:19283 --read-only
```

Then use a simple HTTP client (reqwest) to talk WebDAV internally. This eliminates subprocess startup overhead and enables HTTP range requests for partial hydration.

This is architecturally isolated to the `RcloneBackend` implementation — the `Backend` trait doesn't change.

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
