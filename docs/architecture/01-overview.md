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
- End-to-end encryption of content (rclone provides optional crypt remote)
- P2P sync between local machines (use Syncthing for that)
- GUI / tray icon (follow-on work)

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
│  │  - HydrationQueue: prioritized pending downloads        │ │
│  │  - UploadQueue:    debounced, ordered pending uploads   │ │
│  │  - DeltaPoller:    polls remote for changes             │ │
│  │  - ConflictResolver                                     │ │
│  └──────┬───────────────────────────┬───────────────────────┘ │
│         │                           │                          │
│  ┌──────▼──────────┐   ┌───────────▼──────────────────────┐  │
│  │  state_db       │   │  cache_manager  (src/cache/)      │  │
│  │  (src/state/)   │   │  - Local cache dir (~/.cache/     │  │
│  │  SQLite WAL     │   │    stratosync/)                   │  │
│  │  file_index     │   │  - LRU eviction with quota        │  │
│  │  sync_queue     │   │  - Sparse/partial file support    │  │
│  │  xattr_store    │   └──────────────────────────────────┘  │
│  └──────┬──────────┘                                          │
│         │                                                      │
│  ┌──────▼───────────────────────────────────────────────────┐ │
│  │  backend  (src/backend/)                                 │ │
│  │  rclone subprocess adapter                               │ │
│  │  - lsjson, copy, moveto, delete, about                   │ │
│  │  - streaming stdin/stdout for large transfers            │ │
│  └──────────────────────────────────────────────────────────┘ │
│                                                               │
│  ┌───────────────────┐   ┌──────────────────────────────┐    │
│  │  inotify_watcher  │   │  config  (src/config/)        │    │
│  │  (notify crate)   │   │  TOML, per-mount profiles     │    │
│  └───────────────────┘   └──────────────────────────────┘    │
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
| Daemon crash during upload | WAL entry with `status=UPLOADING` on restart | Re-upload from cache |
| Daemon crash during hydration | WAL entry with `status=HYDRATING` on restart | Delete partial file, re-hydrate on next open |
| Remote ETag conflict on upload | rclone returns non-zero / HTTP 412 | ConflictResolver: create `.conflict` sibling |
| Cache disk full | `ENOSPC` from cache write | Evict LRU entries, retry; if still full, return `ENOSPC` to user |
| rclone binary missing | backend::spawn returns `ENOENT` | Daemon refuses to start; log actionable error |
| Network outage | rclone timeout/error | Exponential backoff; queue persists across restarts |

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
