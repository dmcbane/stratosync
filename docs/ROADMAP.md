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

**Goal**: Power-user capabilities. **Status**: complete for v0.12.0; encrypted caching deferred.

Shipped (Unreleased, in v0.12.0 dev cycle):
- ~~Selective sync (per-mount `ignore_patterns` glob list)~~ ✓ — `.stratosyncignore` in-tree files still pending
- ~~Metrics endpoint (Prometheus-compatible `/metrics` via tokio socket)~~ ✓ — gauge-only; counters (polls/skipped/bytes) deferred
- ~~Bandwidth scheduling (upload only at night, or within configured hours)~~ ✓ — single per-mount window, local-time, wraparound; multi-window and day-of-week deferred
- ~~Multiple accounts for the same provider~~ ✓ — verified already supported via per-mount isolation; documentation added
- ~~File versioning (keep N previous versions, CLI-driven)~~ ✓ — `version_retention` config + `stratosync versions list/restore`; FUSE `.versions/` shadow tree deferred (see follow-ups below)

Deferred to **v0.13.0+**:
- Encrypted caching (encrypt local cache files at rest) — significant crypto work; deserves its own release cycle

### Phase 5 follow-ups (not yet scheduled)

Smaller scope-creep items split out of the main Phase 5 deliverables. Not blocking any release, but called out so they don't get lost.

- **`.stratosyncignore` in-tree files** — gitignore-style cascading ignore files inside the synced tree, on top of the per-mount `ignore_patterns` list. Needs a parser and per-directory caching at lookup time.
- **Selective-sync retroactive cleanup** — a `stratosync ignore prune` CLI that walks the index and removes entries newly matching an ignore pattern (today, ignore rules only prevent NEW indexing).
- **Bandwidth scheduling extensions** — multiple windows per mount, day-of-week selectors, and per-mount Mbps caps surfaced as `bwlimit` rather than going through rclone flags.
- **Metrics counters** — `stratosync_poll_skipped_ignored_total`, `stratosync_polls_total`, and `stratosync_*_bytes_total` for upload/download. Need new atomic counters in the poller and upload queue.
- **FUSE shadow tree for versions** — expose recorded versions as a read-only `.versions/<original_path>/<timestamp>.<ext>` subtree visible in the FUSE mount, so users can browse history with their normal file manager. Today versions are CLI-only. Real implementation work: needs readdir filtering of the shadow prefix (similar to `.stratosync-conflicts/`), retention policy in the cache eviction loop, and conflict-handling for paths that legitimately end in `.versions/` on the remote.
- **Versions enhancements** — bulk restore (`--all` for a directory), diff between versions, and a per-mount size cap for the version-blob store independent of `base_max_file_size`.

---

## Phase 6: File Manager Integration

**Goal**: First-class status overlays and context-menu actions across the major Linux file managers, not just Nautilus. **Status**: GTK-family slice landed (Nautilus / Nemo / Caja); KDE and XFCE still ahead.

All extensions read the same `user.stratosync.{status,etag,remote_path}` xattrs already exposed by the FUSE layer. The current slice shells out to the `stratosync` CLI for actions; if latency ever justifies it, the daemon IPC protocol can grow native pin/unpin/resolve ops without changing the extension surface (the helper API stays put).

Shipped (in v0.12.0 dev cycle):
- **Shared helper** at `contrib/file-managers/common/stratosync_fm_common.py` —
  xattr reading, status→emblem mapping, CLI shell-out, and a single
  `menu_items_for(paths)` generator that drives all three GTK extensions.
  Pure-Python, unit-tested (no `gi` imports).
- **Nautilus extension upgraded**: kept the emblem `InfoProvider`, added a
  `MenuProvider` with Pin / Unpin and three conflict-resolution items
  (keep-local / keep-remote / 3-way merge). Nautilus 4.0 with 3.0 fallback.
- **Nemo (Cinnamon)** — thin wrapper over the shared helper.
- **Caja (MATE)** — thin wrapper over the shared helper.
- **Packaging**: `.deb`, `.rpm`, and AUR PKGBUILD now ship all three
  extensions under their canonical `usr/share/{nautilus,nemo,caja}-python/extensions/` paths, with python3-nautilus / python3-nemo / python3-caja as Recommends/Suggests/optdepends so users only install what their desktop uses.

Deferred to follow-ups:
- **Dolphin (KDE Plasma)** — `KFileItemActionPlugin` (C++/Qt) for context-menu
  actions plus a `KOverlayIconPlugin` for status emblems. Different ecosystem
  from the GTK trio; warrants its own PR.
- **Thunar (XFCE)** — `thunarx` C plugin or `uca` (custom-actions) shim.
  Thunar has no native emblem API, so emblems will need a thumbnailer-based
  status badge or a sidebar panel — design question, not a quick port.
- **PCManFM / PCManFM-Qt (LXDE/LXQt)** — likely custom-actions only.
- **"Copy public link" / "Open remote in browser"** context-menu items —
  need a new `Backend::public_link()` method and a way to map
  `user.stratosync.remote_path` to a browsable URL per provider.
- **Pinned-state xattr** (`user.stratosync.pinned`) so the menu can offer
  Pin *or* Unpin instead of both. Avoided in this slice — the daemon is
  idempotent and there's no UX win worth a daemon change yet.

Acceptance criteria for the GTK slice (met):
- Each extension package installs cleanly under its canonical
  `<fm>-python/extensions/` path on Debian, Fedora, and Arch derivatives.
- Emblems render within one poll cycle of a status change (driven by the
  same `user.stratosync.status` xattr that already powers Nautilus).
- Context-menu actions never block the file manager — every CLI shell-out
  is detached (`Popen` with `start_new_session=True`, no wait).
