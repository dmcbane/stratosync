# Sync Engine & State Machine

## Responsibilities

The sync engine is the coordination center of the daemon. It owns:

- **HydrationQueue** — ordered downloads from remote to cache, with a
  range-read fast path for in-flight hydrations
- **UploadQueue** — deadline-based debounced uploads with optimistic-lock
  ETag check, optional bandwidth-window gating, and post-upload version
  capture
- **RemotePoller** — detects remote changes via delta API (Google Drive
  `pageToken`, OneDrive `/delta`) or full recursive listing; preserves
  selectively-ignored entries so they aren't swept by the stale-entry pass
- **ConflictResolver** — handles ETag mismatches via 3-way text merge
  (when enabled) or `.conflict.*` siblings under `.stratosync-conflicts/`
- **VersionCapture** — content-addressed snapshots before a poller-driven
  cache replace and after each successful upload
- **MutationSerializer** — per-directory locks ensure remote operations
  (rename, delete, mkdir) are ordered correctly

All sync_engine components are async Tokio tasks communicating via channels
or shared state guarded by `Arc<Mutex<…>>`.

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

Upload events are debounced to avoid thrashing on apps that write files in
many small chunks (e.g., text editors saving every few seconds).

The current design is **deadline-based**: every pending upload carries a
single `due_at` instant. There are no per-inode tokio sleep tasks, no abort
mechanism, no oneshot channels. The dispatcher loop wakes on the earliest
deadline. (The pre-v0.8.0 design used abort-and-respawn — multiple sleep
tasks raced to fire and silently dropped uploads under bursty writes.)

```rust
struct UploadQueue {
    // (inode → pending state). Single source of truth.
    pending: Mutex<HashMap<u64, PendingUpload>>,
    upload_window:     Option<UploadWindow>,   // bandwidth schedule
    version_retention: u32,
}

struct PendingUpload {
    cache_path:  PathBuf,
    remote_path: String,
    known_etag:  Option<String>,
    due_at:      Instant,
    immediate:   bool,   // set by fsync; never downgraded by a later Write
    attempt:     u32,
}
```

**Event handling**:

| Event | Effect |
|-------|--------|
| `Write(inode)` (first or subsequent) | Insert/replace `due_at = now + UPLOAD_DEBOUNCE_MS` (default 2000 ms). |
| `Close(inode)` | `due_at = max(due_at, now + UPLOAD_CLOSE_DEBOUNCE_MS)` (default 500 ms). |
| `Fsync(inode)` | `due_at = now`, `immediate = true`. |
| Successful upload | Remove from `pending`. |
| Failed upload (transient) | Re-insert with exponential `due_at` backoff. |

**Bandwidth window gating**. When `upload_window` is configured, the
dispatcher computes `effective_due_at(p) = max(p.due_at, now + secs_until_open)`
for non-immediate jobs, so they sleep until the window opens rather than
spinning. `immediate = true` jobs always dispatch — `fsync()` was an explicit
durability request and shouldn't be overridden by a coarse bandwidth policy.

**Upload execution**:

```
1. Read known_etag from DB.
2. Pre-check: backend::stat() — if remote_etag != known_etag → CONFLICT path.
3. Upload via backend::upload(cache_path, remote_path, if_match=known_etag).
4. Post-check: backend::stat() — if remote_etag changed during upload
   to something other than what we just wrote → CONFLICT path.
5. On success: update DB (etag, status=Cached), capture the just-uploaded
   content as an after_upload version snapshot (if version_retention > 0
   and size <= base_max_file_size), trim history to N entries, then
   release the in_flight slot.
```

---

## RemotePoller

The poller is responsible for detecting changes made on the remote by other clients or other machines.

### Strategy per backend type

```
Google Drive  →  Changes API via pageToken (DeltaProvider: GoogleDriveDelta)
OneDrive      →  /delta endpoint        (DeltaProvider: OneDriveDelta)
Nextcloud     →  PROPFIND with etag     (full listing diff)
S3            →  ListObjectsV2          (full listing diff)
WebDAV        →  PROPFIND               (full listing diff)
SFTP          →  recursive stat scan    (full listing diff)
```

The poller calls `backend.supports_delta()` at startup. When delta is available,
each cycle queries `changes_since(token)`; the backend returns a list of
`Added | Modified | Deleted` events plus the next token. When delta is *not*
available, the poller falls back to `lsjson --recursive --hash` and diffs the
result against an in-memory snapshot of `file_index` (only changed entries
trigger DB writes — idle polls of large mounts are nearly free).

A persistent `change_tokens` table stores the per-mount delta token so the
daemon resumes incrementally after a restart. If the backend rejects a token
(HTTP 410, mapped to `SyncError::TokenExpired`), the poller falls back to one
full listing, obtains a fresh start token, and resumes delta mode on the next
cycle.

```rust
enum RemoteChange {
    Added    { path: String, metadata: RemoteMetadata },
    Modified { path: String, old_etag: String, new_etag: String },
    Deleted  { path: String },
    Renamed  { from: String, to: String },   // not all backends support this
}
```

### Selective-sync filtering and the stale-entry sweep

Both `poll_once` (full-listing) and `poll_once_delta` (delta) check each
incoming entry against the per-mount `Arc<GlobSet>` of `ignore_patterns`.

The full-listing path has a subtlety: phase 3 of `poll_once` runs
`delete_stale_entries`, which removes any DB entry whose generation wasn't
bumped this poll. A naïve "skip ignored entries" implementation would cause
already-indexed entries that *newly* match a pattern to be **deleted** from
the DB and their cache files **wiped**. The poller instead pushes those
inodes into `unchanged_inodes` so their generation gets bumped — they're
treated as "leave alone, just don't update". Net effect: ignore patterns
prevent *new* indexing; they never retroactively unindex (mirroring
gitignore's behavior).

The delta path is simpler — there's no generation sweep, so a `continue`
in the `Added | Modified` arm is correct. `Deleted` arms always apply
(deleting an entry that was never indexed is a no-op).

The poller also caps remote listings at 500 000 entries (DoS protection)
and emits a one-time hint suggesting `ignore_patterns` when the limit is
hit.

### Applying remote changes to open files

If a file is currently `DIRTY` or `UPLOADING` when a remote change arrives:
- If `new_etag != known_etag` AND local has unsaved writes → **CONFLICT**
- If local file is clean (status = CACHED) → capture a `before_poll` version
  snapshot of the cached content (if `version_retention > 0`), then evict
  from cache and set status = STALE

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

## VersionCapture

When `version_retention > 0`, the sync engine records snapshots of file
content at the two moments where data could otherwise be lost:

- **`before_poll`** — invoked from the poller just before invalidating a
  cached local file because the remote ETag changed. The *outgoing* local
  content gets snapshotted.
- **`after_upload`** — invoked from the upload queue after a successful
  upload. The *just-uploaded* content gets snapshotted, so a regretted
  edit can be rolled back.

Snapshots are stored content-addressed in the same `BaseStore` used for the
3-way merge feature (`<cache_dir>/.bases/objects/<sha256[..2]>/<sha256[2..]>`).
A `version_history` row records `(inode, mount_id, object_hash, recorded_at,
file_size, etag, source)`.

Files larger than `[daemon.sync] base_max_file_size` (default 10 MiB) are
skipped — the version cache is for documents and source code, not media
archives.

After each insert, the engine prunes `version_history` to the most recent
`version_retention` rows for that inode. Orphaned blobs (no `base_versions`
*and* no `version_history` references) are deleted from disk via
`BaseStore::remove_object`. The cross-table refcount avoids deleting a blob
that's still serving as a 3-way-merge base.

Restoration is CLI-driven: `stratosync versions restore <path> --index N`
copies the blob over the cache file and marks the entry `Dirty`, so the
next sync uploads the restored content as the new canonical version. The
restored version itself becomes a new `after_upload` history entry on that
upload — there is no "in-place rewind" of history.

---

## Bandwidth window

When `upload_window` is configured for a mount, the upload dispatcher checks
`UploadWindow::contains_minute(now_local)` before firing each non-immediate
job. Outside the window the loop sleeps until
`secs_until_open(now_local)` rather than busy-checking, so a closed window
costs no CPU.

Important properties:

- **Windows are local-time wall-clock** (via `chrono::Local`). Users
  express schedules in terms they actually live by.
- **In-flight uploads are not interrupted** when the window closes —
  cancelling mid-upload risks corrupted partial state on the remote;
  letting them finish is safer and the bandwidth difference is small.
- **`fsync()` always bypasses** (`PendingUpload::immediate = true`).
- **Polling, hydration, and pinning are not gated** — only uploads are.
- The degenerate `"HH:MM-HH:MM"` (start == end) is treated as
  always-open, not always-closed (less surprising for typos).

---

## Sync on Startup

At daemon start:
1. Load all `DIRTY` and `UPLOADING` entries from DB → re-queue uploads.
2. Load all `HYDRATING` entries → delete partial cache files, reset to `REMOTE`.
3. Migrate any pre-existing conflict siblings into the
   `.stratosync-conflicts/` namespace (one-time tree walk).
4. Validate `ignore_patterns` and build the per-mount `GlobSet`. Bad
   patterns abort startup with a clear error.
5. Run one full remote poll to detect offline changes (delta token is
   used when present; otherwise a full listing).
6. Resume normal operation.

This ensures the daemon converges after any crash without requiring user
intervention.
