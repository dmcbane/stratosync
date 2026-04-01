# Sync Engine & State Machine

## Responsibilities

The sync engine is the coordination center of the daemon. It owns:

- **HydrationQueue** — ordered downloads from remote to cache
- **UploadQueue** — debounced, ETag-checked uploads from cache to remote  
- **RemotePoller** — detects remote changes (polling or delta API)
- **ConflictResolver** — handles ETag mismatches and concurrent edits
- **MutationSerializer** — ensures remote operations are ordered correctly

All sync_engine components are async Tokio tasks communicating via channels.

---

## File State Machine

```
                         ┌──────────────────────────────┐
                         │                              │
                         ▼                              │ remote change detected
                      REMOTE ───────────────────► STALE │
                         │                         │    │
                  open() │                         │ open()
                         │                    hydrate   │
                         ▼                         │    │
                    HYDRATING ◄────────────────────┘    │
                    │       │                           │
                success   failure                       │
                    │       │                           │
                    ▼       ▼                           │
                 CACHED   REMOTE  (retry later)         │
                    │                                   │
             write()│                                   │
                    ▼                                   │
                  DIRTY ──────────────────────────► CONFLICT
                    │         ETag mismatch              │
              debounce                                  │
               window                          conflict resolution
               expires                                  │
                    ▼                                   ▼
                UPLOADING ──── success ──────────► CACHED
                    │
                  failure (network error etc)
                    │
                    ▼
                  DIRTY (re-queued with backoff)
```

### State Transition Rules

| From | To | Trigger | Action |
|------|----|---------|--------|
| REMOTE | HYDRATING | `open()` on uncached file | Lock inode, start download |
| HYDRATING | CACHED | download complete | Write to cache, unlock inode |
| HYDRATING | REMOTE | download failed | Clean up partial, log error |
| CACHED | DIRTY | any `write()` or `create()` | Mark in DB, start debounce timer |
| CACHED | STALE | remote poller sees newer ETag | Invalidate cache entry |
| STALE | HYDRATING | next `open()` | Re-download |
| DIRTY | UPLOADING | debounce window expires OR `fsync()` | Start upload |
| UPLOADING | CACHED | upload success, ETag matches | Update ETag in DB |
| UPLOADING | CONFLICT | upload fails with ETag mismatch | Run ConflictResolver |
| UPLOADING | DIRTY | upload fails with network error | Backoff, re-queue |
| CONFLICT | CACHED | conflict file written, canonical resolved | Update DB |

---

## HydrationQueue

```rust
struct HydrationQueue {
    // Priority queue: foreground opens get higher priority than background prefetch
    queue: BinaryHeap<HydrationJob>,
    in_flight: HashMap<u64, JoinHandle<()>>,   // inode → running task
    max_concurrent: usize,                      // default: 4
}

struct HydrationJob {
    inode:    u64,
    priority: HydrationPriority,
    notify:   Option<oneshot::Sender<Result<(), SyncError>>>,
}

enum HydrationPriority {
    Foreground = 2,   // user is blocked waiting for open()
    Background = 1,   // prefetch heuristic
    Prefetch   = 0,   // speculative (e.g., open directory → prefetch small files)
}
```

When a foreground hydration request arrives for an inode already being hydrated at background priority, the existing task is upgraded in priority (rclone transfer rate-limit hint via `--bwlimit`).

---

## UploadQueue

Upload events are debounced to avoid thrashing on apps that write files in many small chunks (e.g., text editors saving every few seconds).

```rust
struct UploadQueue {
    pending: HashMap<u64, UploadJob>,   // inode → job
    timers:  JoinSet<u64>,              // tokio::time::sleep tasks
}

struct UploadJob {
    inode:       u64,
    cache_path:  PathBuf,
    remote_path: String,
    known_etag:  Option<String>,   // ETag we last synced; used for optimistic lock
    enqueued_at: Instant,
    attempt:     u32,
}
```

**Debounce logic**:
- On first `write()`: insert job with `due_at = now + DEBOUNCE_WINDOW` (default 2s).
- On subsequent `write()` for same inode: reset `due_at`.
- On `fsync()`: set `due_at = now` (immediate).
- On `close()`: set `due_at = now + SHORT_WINDOW` (0.5s).

**Upload execution**:
```
1. Read known_etag from DB
2. Call backend::get_etag(remote_path)
3. If remote_etag != known_etag → CONFLICT path
4. Call backend::upload(cache_path, remote_path, if_match=known_etag)
5. If HTTP 412 Precondition Failed → CONFLICT path
6. On success: update DB with new etag, status=CACHED
```

---

## RemotePoller

The poller is responsible for detecting changes made on the remote by other clients or other machines.

### Strategy per backend type

```
Google Drive  →  Changes API (pageToken, poll interval 30s)
OneDrive      →  Delta API (/delta endpoint, poll interval 30s)
SharePoint    →  Delta API
Nextcloud     →  PROPFIND with etag, poll interval 60s
S3            →  ListObjectsV2 with ETag compare, poll interval 120s
WebDAV        →  PROPFIND with Last-Modified/ETag, poll interval 60s
SFTP          →  recursive stat scan, poll interval 300s
```

rclone exposes `lsjson --recursive --hash` for most backends. We diff the result against our `file_index` to detect changes.

```rust
async fn poll_once(backend: &Backend, db: &StateDb) -> Vec<RemoteChange> {
    let remote_listing = backend.lsjson_recursive().await?;
    let local_index    = db.snapshot_remote_metadata().await?;
    diff_listings(local_index, remote_listing)
}

enum RemoteChange {
    Added    { path: String, metadata: RemoteMetadata },
    Modified { path: String, old_etag: String, new_etag: String },
    Deleted  { path: String },
    Renamed  { from: String, to: String },   // not all backends support this
}
```

For Google Drive and OneDrive, we maintain a `change_token` (pageToken / deltaLink) in the DB to avoid full re-scans.

### Applying remote changes to open files

If a file is currently `DIRTY` or `UPLOADING` when a remote change arrives:
- If `new_etag != known_etag` AND local has unsaved writes → **CONFLICT**
- If local file is clean (status = CACHED) → evict from cache, set status = STALE

---

## ConflictResolver

Conflict resolution is based on the Syncthing/Nextcloud principle: **never silently discard data**.

```rust
async fn resolve_conflict(
    inode: &FileEntry,
    local_cache: &Path,
    remote_metadata: &RemoteMetadata,
    db: &StateDb,
    backend: &Backend,
) -> Result<()> {
    // 1. Generate conflict filename
    let conflict_name = format!(
        "{}.conflict.{}.{}",
        inode.stem(),
        Utc::now().format("%Y%m%dT%H%M%SZ"),
        &inode.etag.as_deref().unwrap_or("unknown")[..8]
    );
    // e.g. "report.conflict.20250315T142301Z.abc12345.pdf"

    // 2. Upload local version under conflict name
    let conflict_remote = inode.remote_path.with_name(&conflict_name);
    backend.upload(local_cache, &conflict_remote, None).await?;

    // 3. Fetch the winning (remote) version
    backend.download(&inode.remote_path, local_cache).await?;

    // 4. Update DB: canonical inode points to remote version,
    //              create a new inode for the conflict file
    db.insert_conflict_file(inode, &conflict_name).await?;
    db.update_status(inode.inode, HydrationStatus::Cached).await?;

    // 5. Emit a desktop notification (if notify-send available)
    notify_conflict(&inode.name, &conflict_name);

    Ok(())
}
```

**Conflict filename anatomy**: `{stem}.conflict.{iso8601}.{etag_prefix}{ext}`

This matches the pattern from Nextcloud desktop client ("conflicted copy") while embedding enough information to correlate which version was which.

### Future: 3-way merge for text files

For text files (.md, .txt, .rs, etc.), we can attempt a 3-way merge using the common ancestor (last known version in DB). This is optional behavior gated by config:

```toml
[sync]
text_conflict_strategy = "merge"   # or "keep_both" (default)
```

---

## MutationSerializer

Remote mutations (create, delete, rename, mkdir, rmdir) must be serialized per-directory to prevent races. We maintain a per-directory async mutex:

```rust
// Arc<Mutex<()>> per directory inode
directory_locks: DashMap<u64, Arc<Mutex<()>>>
```

Mutations are queued and executed in order, preventing the pathological case where a rename followed by a delete arrives out-of-order at the remote.

---

## Sync on Startup

At daemon start:
1. Load all `DIRTY` and `UPLOADING` entries from DB → re-queue uploads.
2. Load all `HYDRATING` entries → delete partial cache files, reset to `REMOTE`.
3. Run one full remote poll to detect offline changes.
4. Resume normal operation.

This ensures the daemon converges after any crash without requiring user intervention.
