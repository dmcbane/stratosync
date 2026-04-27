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

**Goal**: First-class status overlays and context-menu actions across the major Linux file managers, not just Nautilus. **Status**: context-menu actions shipped on every targeted file manager (Nautilus / Nemo / Caja / Dolphin / Konqueror / Thunar / PCManFM / PCManFM-Qt). Emblems shipped on the GTK trio; emblem support on Dolphin and an emblem alternative for Thunar/PCManFM are still ahead.

All extensions read the same `user.stratosync.{status,etag,remote_path}` xattrs already exposed by the FUSE layer. Actions shell out to the `stratosync` CLI; if latency ever justifies it, the daemon IPC protocol can grow native pin/unpin/resolve ops without changing the extension surface.

Shipped (in v0.12.0 dev cycle):
- **Shared Python helper** at `contrib/file-managers/common/stratosync_fm_common.py` —
  xattr reading, status→emblem mapping, CLI shell-out, and a single
  `menu_items_for(paths)` generator that drives all three GTK extensions.
  Pure-Python, unit-tested (no `gi` imports).
- **Shared shell wrapper** `contrib/file-managers/bin/stratosync-fm-action` —
  used by every declarative-action file manager (Dolphin, Thunar, PCManFM).
  Checks `user.stratosync.status` xattr is present, dispatches to the right
  `stratosync` subcommand, surfaces errors via `notify-send`, detaches via
  `setsid` so the FM never blocks.
- **Nautilus (GNOME)** — emblem `InfoProvider` + `MenuProvider` with Pin /
  Unpin and three conflict-resolution items. Nautilus 4.0 with 3.0
  fallback.
- **Nemo (Cinnamon)** — thin wrapper over the shared Python helper.
- **Caja (MATE)** — thin wrapper over the shared Python helper.
- **Dolphin / Konqueror (KDE)** — KIO ServiceMenu (`.desktop`) under a
  *Stratosync* submenu. Five context-menu items, no compilation.
- **PCManFM / PCManFM-Qt (LXDE/LXQt)** — FreeDesktop file-manager Actions
  spec, one `.desktop` per item under `/usr/share/file-manager/actions/`.
- **Thunar (XFCE)** — UCA (Custom Actions) snippet shipped under
  `share/doc/stratosync/thunar/` for manual merge into
  `~/.config/Thunar/uca.xml`. Auto-merge would clobber pre-existing user
  actions.
- **Packaging**: `.deb`, `.rpm`, and AUR PKGBUILD ship every extension and
  the wrapper binary at the canonical XDG paths; missing Python bindings
  are only Recommends/Suggests, never required.

Deferred to follow-ups:
- **Dolphin emblem overlays** — `KOverlayIconPlugin` (C++/Qt + KIO + CMake).
  Real build, not a config file. Tracked separately.
- **Thunar / PCManFM emblems** — neither has a native emblem API. Options
  on the table: thumbnailer-based status badges, sidebar-panel widget,
  or a no-op (acceptable; the CLI/dashboard already cover the case where
  you want to *see* status).
- **Smart filtering on declarative menus** — Dolphin / Thunar / PCManFM
  always show the *Stratosync* items because their action specs can't
  read xattrs. The wrapper handles non-stratosync paths gracefully via
  `notify-send`, but the items *are* there. A KIO `KAbstractFileItemActionPlugin`
  could filter based on the xattr in C++; punted with the rest of the KDE
  C++ work.
- **"Copy public link" / "Open remote in browser"** context-menu items —
  need a new `Backend::public_link()` method and a way to map
  `user.stratosync.remote_path` to a browsable URL per provider.
- **Pinned-state xattr** (`user.stratosync.pinned`) so the menu can offer
  Pin *or* Unpin instead of both. Avoided in this slice — the daemon is
  idempotent and there's no UX win worth a daemon change yet.

Acceptance criteria (met for the action-only scope):
- Every targeted file manager package installs cleanly under its canonical
  XDG path on Debian, Fedora, and Arch derivatives.
- Emblems render within one poll cycle of a status change on the GTK trio
  (Nautilus / Nemo / Caja).
- Context-menu actions never block the file manager — Python wrappers use
  `Popen` with `start_new_session=True`; declarative wrappers use
  `setsid stratosync … &`.
- Acting on a non-stratosync path through any of the declarative menus
  surfaces a desktop notification rather than failing silently.
