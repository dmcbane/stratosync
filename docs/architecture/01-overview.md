# Architecture Overview

## 1. Goals and Non-Goals

### Goals
- **On-demand hydration**: Files appear in the VFS immediately; data is only transferred when a process opens a file.
- **Bidirectional sync**: Local writes propagate to the remote; remote changes appear locally (via polling or delta API).
- **Multi-backend**: Any rclone-supported provider works without daemon changes.
- **Conflict preservation**: When a concurrent write collision is detected, both versions are preserved with unambiguous names; data is never silently discarded.
- **Crash-safe**: Every state transition is journaled; a daemon restart after crash converges to a consistent state.
- **Low overhead**: Idle CPU and memory should be negligible; inotify-driven (not polling) for local changes.

### Non-Goals (for now)
- Real-time collaborative editing (CRDTs, OT)
- End-to-end encryption of content (rclone provides optional crypt remote;
  encrypted local cache is on the v0.13.0 roadmap separately)
- P2P sync between local machines (use Syncthing for that)

---

## 2. Component Map

```
┌──────────────────────────────────────────────────────────────┐
│  User process (any app)                                       │
│    open("/mnt/cloud/doc.pdf")  read()  write()  close()      │
└──────────────────────┬───────────────────────────────────────┘
                       │  Linux kernel VFS
┌──────────────────────▼───────────────────────────────────────┐
│  FUSE kernel module  (fuse.ko)                                │
└──────────────────────┬───────────────────────────────────────┘
                       │  /dev/fuse  (userspace messages)
┌──────────────────────▼───────────────────────────────────────┐
│  stratosync daemon                                            │
│                                                               │
│  ┌─────────────────────────────────────────────────────────┐ │
│  │  fuse_layer  (src/fuse/)                                │ │
│  │  Implements: lookup, getattr, readdir, open, read,      │ │
│  │              write, release, fsync, setxattr, getxattr  │ │
│  │  Uses fuser crate; runs in a dedicated thread pool      │ │
│  └──────────────────────────┬──────────────────────────────┘ │
│                             │  async channels (tokio)         │
│  ┌──────────────────────────▼──────────────────────────────┐ │
│  │  sync_engine  (src/sync/)                               │ │
│  │  - HydrationQueue: foreground/background, range-read    │ │
│  │  - UploadQueue:    deadline-based debounce + bw window  │ │
│  │  - RemotePoller:   delta API or full listing per backend│ │
│  │  - ConflictResolver: 3-way merge or .conflict siblings  │ │
│  │  - VersionCapture: pre-replace / post-upload snapshots  │ │
│  └──────┬───────────────────────────┬───────────────────────┘ │
│         │                           │                          │
│  ┌──────▼──────────┐   ┌───────────▼──────────────────────┐  │
│  │  state_db       │   │  cache_manager  (src/cache/)      │  │
│  │  (per-mount     │   │  - Cache dir per mount            │  │
│  │   SQLite WAL)   │   │    (~/.cache/stratosync/<name>/)  │  │
│  │  file_index     │   │  - LRU eviction (pin-aware)       │  │
│  │  sync_queue     │   │  - .bases/objects/  content-      │  │
│  │  base_versions  │   │    addressed BaseStore (3-way     │  │
│  │  version_history│   │    merge + version history)       │  │
│  │  delete_tomb…   │   │  - Range-read fast path           │  │
│  │  change_tokens  │   │    (rclone cat --offset/--count)  │  │
│  │  xattr_store    │   └──────────────────────────────────┘  │
│  └──────┬──────────┘                                          │
│         │                                                      │
│  ┌──────▼───────────────────────────────────────────────────┐ │
│  │  backend  (src/backend/)                                 │ │
│  │  - RcloneBackend: command-mode subprocess                │ │
│  │  - WebDavSidecarBackend: long-running rclone serve webdav│ │
│  │  - DeltaProvider: GoogleDriveDelta, OneDriveDelta        │ │
│  └──────────────────────────────────────────────────────────┘ │
│                                                               │
│  ┌──────────────────┐  ┌─────────────────┐ ┌──────────────┐  │
│  │ inotify_watcher  │  │  metrics        │ │ ipc_socket   │  │
│  │ (selective-sync- │  │  /metrics HTTP  │ │ (dashboard,  │  │
│  │  filtered)       │  │  (opt-in)       │ │  CLI status) │  │
│  └──────────────────┘  └─────────────────┘ └──────────────┘  │
└──────────────────────────────────────────────────────────────┘
                       │  rclone subprocess
┌──────────────────────▼───────────────────────────────────────┐
│  rclone (external binary)                                     │
│  Google Drive  ·  OneDrive  ·  S3  ·  WebDAV  ·  ...        │
└──────────────────────────────────────────────────────────────┘
```

---

## 3. Data Flow: Reading a File (Cache Miss)

```
User process           FUSE layer         Sync Engine        rclone
     │                     │                   │                │
     │  open("doc.pdf")    │                   │                │
     │────────────────────►│                   │                │
     │                     │ lookup state_db   │                │
     │                     │ → status=REMOTE   │                │
     │                     │                   │                │
     │                     │ enqueue_hydration │                │
     │                     │──────────────────►│                │
     │                     │                   │ rclone copy    │
     │                     │                   │───────────────►│
     │                     │  (FUSE blocks      │                │
     │                     │   open() here)     │◄───────────────│
     │                     │                   │ update state_db│
     │                     │◄──────────────────│                │
     │◄────────────────────│                   │                │
     │  fd = 3             │                   │                │
     │                     │                   │                │
     │  read(fd, buf, n)   │                   │                │
     │────────────────────►│                   │                │
     │                     │  pread(cache_fd)  │                │
     │◄────────────────────│                   │                │
     │  [data]             │                   │                │
```

---

## 4. Data Flow: Writing a File

```
User process           FUSE layer         Sync Engine        rclone
     │                     │                   │                │
     │  write(fd, buf)     │                   │                │
     │────────────────────►│                   │                │
     │                     │  write to         │                │
     │                     │  cache file       │                │
     │                     │  mark DIRTY       │                │
     │◄────────────────────│                   │                │
     │                     │                   │                │
     │  close(fd)          │                   │                │
     │────────────────────►│                   │                │
     │                     │  enqueue_upload   │                │
     │                     │  (debounced 2s)   │                │
     │◄────────────────────│                   │                │
     │                     │           [timer fires]            │
     │                     │                   │ fetch remote   │
     │                     │                   │ etag           │
     │                     │                   │───────────────►│
     │                     │                   │◄───────────────│
     │                     │                   │ etag matches?  │
     │                     │                   │ YES → upload   │
     │                     │                   │───────────────►│
     │                     │                   │◄───────────────│
     │                     │                   │ update state   │
```

---

## 5. Threading Model

The daemon uses a hybrid threading model:

- **FUSE thread pool** (OS threads, ~4): handles `/dev/fuse` messages synchronously. Blocking here blocks the user process, so hydration is handled by sending a message to the async runtime and parking the FUSE thread on a `oneshot` channel.
- **Tokio async runtime** (1 thread per core): owns sync_engine, backend calls, state_db writes, cache_manager.
- **inotify thread** (1 OS thread): feeds events into a tokio mpsc channel.

The key design rule: **FUSE threads never do I/O directly**. They enqueue work and wait.

---

## 6. Failure Modes and Recovery

| Failure | Detection | Recovery |
|---------|-----------|----------|
| Daemon crash during upload | `status=UPLOADING` row on restart | Reset to `Dirty`, re-upload from cache |
| Daemon crash during hydration | `status=HYDRATING` row on restart | Delete partial file, reset to `Remote`; re-hydrate on next open |
| Remote ETag conflict on upload | Pre/post-upload ETag mismatch | ConflictResolver: 3-way merge for text (when enabled), else `.conflict.*` sibling under `.stratosync-conflicts/` |
| Cache disk full | `ENOSPC` from cache write | LRU evict; if still full, return `ENOSPC` to user |
| rclone binary missing | `backend::which_rclone()` returns `ENOENT` | Daemon refuses to start; log actionable error |
| Network outage / 5xx | rclone timeout/error | Exponential backoff (3 fails → double interval, cap 10 min); queue persists across restarts |
| GDrive 403 rate-limit | HTTP 403 with `rateLimitExceeded` in body | Mapped to `Transient`, backoff applied (not `PermissionDenied`) |
| OAuth token expired | HTTP 401 from delta API | Force-refresh via `rclone about`, retry once |
| Change token invalidated (HTTP 410) | Delta API returns `TokenExpired` | Fall back to one full listing, obtain fresh token, resume delta mode |
| Locally-deleted file resurrects from poller | Tombstone in `delete_tombstones` blocks the upsert (5 min) | None needed; tombstone expires after the remote delete confirms |

---

## 7. Key Design Decisions

### Why rclone instead of direct API clients?

Implementing even one provider's API correctly (OAuth refresh, chunked upload, resumable upload, rate limiting, delta tokens) is weeks of work. rclone handles all of this and is battle-tested across 70+ providers. The subprocess overhead (~30ms cold-start per operation) is acceptable because we pipeline operations and maintain a long-running rclone serve process for bulk transfers.

### Why SQLite instead of a custom binary format?

Crash safety via WAL mode is free. Schema evolution is straightforward. The query interface is expressive enough to implement the sync queue, LRU eviction, and xattr store without specialized data structures. At the scale of a personal sync daemon (< 1M files), SQLite is fast enough — index reads are sub-millisecond.

### Why FUSE3 instead of NFS or 9P?

FUSE3 is the only standard option that requires zero kernel patches, works as non-root with `allow_other`, and gives us full control over each VFS operation. NFS would require a server process and has poor support for on-demand semantics. 9P (via `virtio-9p` or `v9fs`) is kernel-mount only.

### Conflict resolution philosophy (from Nextcloud/Syncthing analysis)

Both projects agree: **never silently discard data**. Syncthing renames the losing file to `.sync-conflict-<timestamp>-<deviceid>`; Nextcloud suffixes with `(conflicted copy <date>)`. We adopt a similar approach: the remote wins the canonical path; the local version becomes `filename.conflict.<timestamp>.<truncated-hash>`.
