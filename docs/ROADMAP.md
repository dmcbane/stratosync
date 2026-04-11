# Implementation Roadmap

## Phase 1: Read-Only VFS (MVP)

**Goal**: Mount a remote as a read-only FUSE filesystem. Files hydrate on-demand. No writes, no sync.

Deliverables:
- `stratosync-core` crate: `Backend` trait + `RcloneBackend`
- `stratosync-core`: `StateDb` (file_index, mounts tables only)
- `stratosync-daemon` crate: FUSE mount (lookup, getattr, readdir, open, read, release)
- `stratosync-cli` crate: `stratosync mount`, `stratosync umount`, `stratosync ls`
- Basic config loading from TOML
- Systemd user service unit file

Acceptance criteria:
- `ls ~/GoogleDrive/` works
- `cat ~/GoogleDrive/Documents/report.pdf` downloads and streams
- `vlc ~/GoogleDrive/Videos/movie.mkv` plays (full hydration before play)
- Daemon survives process restart; re-mounts on systemd start

---

## Phase 2: Bidirectional Sync

**Goal**: Local writes propagate to remote. Remote changes appear locally.

Deliverables:
- FUSE write, create, mkdir, unlink, rmdir, rename operations
- UploadQueue with debounce
- RemotePoller (polling strategy first; delta API as enhancement)
- CacheManager with LRU eviction
- sync_queue table; startup recovery
- `stratosync status` CLI showing per-file sync state

Acceptance criteria:
- Edit a file in `~/GoogleDrive/` → appears on Google Drive within 5 seconds
- Create/delete/rename works bidirectionally
- Daemon survives crash mid-upload and retries on restart
- Cache stays under configured quota

---

## Phase 3: Conflict Resolution & Safety

**Goal**: True conflicts are handled gracefully; no data loss under any scenario.

Deliverables:
- ~~ConflictResolver with `.conflict.{ts}.{hash}` naming~~ ✓ (v0.1.0)
- ~~Optimistic-lock upload (ETag check before and after)~~ ✓ (v0.1.0)
- ~~Content-hash ETag detection (SHA-1/MD5 instead of file IDs)~~ ✓ (v0.8.0)
- ~~3-way text merge via `git merge-file` with base version store~~ ✓ (v0.8.0)
- ~~`stratosync conflicts list`~~ ✓ (v0.1.0) / ~~`resolve` CLI (keep-local, keep-remote, merge, diff)~~ ✓ (v0.9.0)
- ~~Desktop notification on conflict and upload failures (notify-send)~~ ✓ (v0.8.0/v0.9.0)
- ~~xattr exposure of sync status (`user.stratosync.{status,etag,remote_path}`)~~ ✓ (v0.9.0)
- ~~Change token support for GDrive (pageToken) and OneDrive (deltaLink)~~ ✓ (v0.7.0/v0.7.1)

Acceptance criteria:
- ~~Simulate concurrent edit from two machines; conflict file appears within one poll cycle~~ ✓ (live-tested v0.8.0)
- ~~`getfattr -n user.stratosync.status` works on any file in the mount~~ ✓ (v0.9.0)
- ~~3-way text merge resolves clean conflicts automatically (when enabled)~~ ✓ (live-tested v0.8.0)

---

## Phase 4: Performance & UX Polish

**Goal**: Fast enough for daily use; desktop integration.

Deliverables:
- ~~`rclone serve webdav` sidecar for low-latency transfers~~ ✓ (v0.11.0)
- ~~Partial/range hydration for large files (`download_range` via `rclone cat --offset/--count`)~~ ✓ (v0.10.0)
- ~~Prefetch heuristics (readdir → prefetch small files under configurable threshold)~~ ✓ (v0.10.0)
- ~~`stratosync pin` / `stratosync unpin` for offline availability~~ ✓ (v0.10.0)
- ~~GNOME/Nautilus emblem extension (Python, reads `user.stratosync.status` xattr)~~ ✓ (v0.11.0)
- ~~Tray indicator (`stratosync-tray` via `ksni` StatusNotifierItem)~~ ✓ (v0.11.0)
- ~~Packaging: Debian `.deb`, Fedora `.rpm`, Arch AUR~~ ✓ (v0.11.0)

---

## Phase 5: Advanced Features

**Goal**: Power-user capabilities.

Potential additions:
- Selective sync (ignore patterns per mount, `.stratosyncignore`)
- File versioning (keep N previous versions in a `.versions/` shadow tree)
- Bandwidth scheduling (upload only at night, or within configured hours)
- Metrics endpoint (Prometheus-compatible `/metrics` via tokio socket)
- Multiple accounts for the same provider (e.g. two Google Drive accounts)
- Encrypted caching (encrypt local cache files at rest)
