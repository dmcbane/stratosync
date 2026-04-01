# FUSE Layer Design

## Overview

The FUSE layer is the daemon's public interface to the kernel VFS. It translates kernel requests into internal operations and must maintain two invariants:

1. **Posix correctness**: `stat()` returns accurate sizes; `read()` returns accurate data; `open()` succeeds or blocks until data is available.
2. **Non-blocking for cached files**: If a file is already in local cache and not dirty, `open()`/`read()` must be as fast as a local filesystem.

## Crate: `fuser`

We use the [`fuser`](https://crates.io/crates/fuser) crate (the maintained fork of `fuse-rs`), which provides safe Rust bindings to FUSE3's userspace ABI.

```toml
[dependencies]
fuser = "0.14"
```

## Inode Model

Every filesystem entry has an inode number allocated by the daemon. Inodes are stored in `state_db.file_index` and persist across daemon restarts.

```
inode(u64) → FileEntry {
    inode:       u64,
    parent:      u64,
    name:        String,
    remote_path: String,       // as rclone sees it: "gdrive:Documents/report.pdf"
    kind:        FileKind,     // File | Directory | Symlink
    size:        u64,          // from remote metadata
    mtime:       SystemTime,
    etag:        Option<String>,
    status:      HydrationStatus,
    cache_path:  Option<PathBuf>,
}

enum HydrationStatus {
    Remote,       // metadata only, no local data
    Hydrating,    // download in progress
    Cached,       // local cache is fresh
    Dirty,        // local cache has unsynced changes
    Uploading,    // upload in progress
    Conflict,     // conflicting versions exist
}
```

Inode 1 is always the root of the mount. Directory children are lazily populated on the first `readdir`.

## Implemented Operations

### `lookup(parent, name) → FileAttr`

1. Query `file_index` for `(parent_inode, name)`.
2. If not found and parent is `status=Remote`: trigger a `list_children` backend call, populate children, retry.
3. Return `FileAttr` with `ino`, `size`, `kind`, `atime/mtime/ctime`.

Cache the `FileAttr` in memory with a short TTL (default 5s) to avoid DB roundtrips on repeated `stat()` calls.

### `getattr(ino) → FileAttr`

Direct DB lookup + memory cache.

### `readdir(ino, offset) → Vec<(ino, name, kind)>`

If directory `status=Remote`, call `backend::list(remote_path)`, insert children into `file_index`, then serve from DB. Mark directory `status=Cached` with a `dir_mtime`.

Uses offset-based pagination: entries are stable-sorted by inode so that `seekdir`/`telldir` is consistent.

### `open(ino, flags) → FileHandle`

This is the critical path:

```
1. Load FileEntry from DB
2. If status == Cached and cache is fresh → allocate FileHandle, return immediately
3. If status == Dirty → allocate FileHandle over existing cache file
4. If status == Remote:
   a. Set status = Hydrating (in DB, within a transaction)
   b. Send HydrationRequest to sync_engine via oneshot
   c. PARK THIS FUSE THREAD waiting on the oneshot response channel
   d. On response: if Ok → status=Cached, allocate FileHandle
                   if Err → return EREMOTEIO or ETIMEDOUT
5. If status == Hydrating (another thread beat us):
   a. Wait on a per-inode Condvar until status transitions out of Hydrating
```

The parking-on-oneshot pattern is critical. FUSE threads are blocking by nature; we cannot use `.await`. We use `oneshot::channel` from `tokio::sync` and call `rx.blocking_recv()`.

FileHandle is a `u64` token that maps to:
```rust
struct OpenFile {
    inode:     u64,
    cache_fd:  File,        // open file descriptor into cache
    write_buf: Option<Vec<u8>>,
    flags:     OpenFlags,
}
```

### `read(fh, offset, size) → Vec<u8>`

`pread(cache_fd, size, offset)`. No daemon logic needed if hydrated.

For large files with partial-hydration (future work), check if the byte range is in the range map; if not, trigger a range-hydration request.

### `write(fh, offset, data) → u32`

`pwrite(cache_fd, data, offset)`. Mark inode `status=Dirty` in DB. Enqueue an upload event (debounced) to sync_engine.

Return `data.len()` as bytes written.

### `release(fh)`

Flush write buffer. If file is `Dirty`, finalize the debounce: if no further writes in the configured window (default 2s), the upload fires.

Drop the `OpenFile` entry from the fh map.

### `fsync(fh, datasync)`

Force-flush: bypass the debounce window, enqueue an immediate upload. Block until upload completes (same oneshot pattern as `open`).

This makes `fsync` semantically equivalent to "I want this on the remote right now," which matches user expectations when an app explicitly syncs.

### `mkdir` / `create` / `unlink` / `rmdir` / `rename`

All mutating operations:
1. Apply locally (update DB, create/delete cache entries).
2. Enqueue a remote mutation operation to sync_engine.
3. Mutations are serialized through a per-directory lock to prevent ordering races.

`rename` is the most complex — if cross-directory, requires an atomic remote rename. rclone exposes `moveto` for this. If the remote doesn't support server-side move (e.g. some WebDAV implementations), fall back to copy+delete.

### `getxattr` / `setxattr` / `listxattr`

Used to expose sync metadata to user tools:

```bash
getfattr -n user.stratosync.status ~/Cloud/doc.pdf
# → "cached"

getfattr -n user.stratosync.etag ~/Cloud/doc.pdf  
# → "\"abc123\""

getfattr -n user.stratosync.remote_path ~/Cloud/doc.pdf
# → "gdrive:Documents/doc.pdf"
```

Stored in the `xattr_store` table in state_db.

## File Handle Table

```rust
// Global, protected by DashMap for lock-free concurrent access
static FILE_HANDLES: DashMap<u64, Arc<Mutex<OpenFile>>> = ...;
static NEXT_FH: AtomicU64 = AtomicU64::new(1);
```

## Timeout Tuning

FUSE attribute and entry timeouts control how long the kernel caches our responses:

```rust
MountOption::AutoUnmount,
// Entry timeout: how long kernel caches lookup results
// Keep short for remote dirs (content may change), longer for local cache
let entry_timeout = Duration::from_secs(5);
let attr_timeout = Duration::from_secs(5);
```

For directories known to be fully synced, we can extend these to 60s.

## Notes from google-drive-ocamlfuse

The OCaml FUSE driver for GDrive taught us:
- **Directory listing is expensive**: cache aggressively with a `readdir_cache` with explicit invalidation on any mutation.
- **GDrive doesn't have a real mtime for directories**: synthesize from the most recent child mtime.
- **inode stability matters**: apps like `vim` re-open files by inode after a rename. Store inodes in DB keyed on remote path, never re-assign.
