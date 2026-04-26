# Changelog

All notable changes to this project will be documented in this file.

## [0.12.0] - unreleased

### Added
- **Dashboard TUI** (`stratosync dashboard`): live ratatui-based view of
  per-mount sync state, hydration queue, upload queue, and poller status.
  Backed by a new daemon IPC socket that aggregates per-mount status from
  the existing state DB and queue handles.
- **Conflicts cleanup CLI** (`stratosync conflicts cleanup`): walks the
  conflict namespace, drops siblings whose content is byte-equal to the
  canonical file (the common case after a transient false-positive),
  shows per-entry progress, and accepts `--dry-run`. Includes a
  stat-based fast path and live progress output for large trees.
- **File versioning (Phase 5, item 5)**: per-mount
  `version_retention` (default 10) records snapshots of file content
  at the moments it would otherwise be lost — local cache content
  before a poller-detected remote change replaces it (`before_poll`),
  and the just-uploaded content after a successful upload
  (`after_upload`). Snapshots reuse the existing content-addressed
  `BaseStore`, so identical content across versions deduplicates
  automatically. New `stratosync versions list <path>` and
  `stratosync versions restore <path> --index N` CLI commands let
  users recover prior versions; restore copies the blob back to the
  cache and marks the file Dirty for re-upload. Files larger than
  `[daemon.sync] base_max_file_size` are skipped to bound disk
  usage. Set `version_retention = 0` to disable.
- **Multiple accounts per provider (Phase 5, item 4)**: documented as
  already-supported. Each `[[mount]]` block is fully isolated — its
  own SQLite DB, cache directory, poller, upload queue, FUSE thread,
  and (with `webdav_sidecar = true`) WebDAV sidecar on a distinct
  port. Configuring two Google Drive accounts is a matter of two
  rclone remotes plus two mount blocks. No code change.
- **Bandwidth scheduling (Phase 5, item 3)**: per-mount
  `upload_window = "HH:MM-HH:MM"` (local time, wraparound supported).
  Outside the window the upload queue holds dirty files but does not
  dispatch; the loop sleeps until the window reopens, no busy-wait. In-
  flight uploads are not interrupted. `fsync()` always bypasses the
  schedule — the user explicitly asked for durability, the bandwidth
  policy is coarse and shouldn't override that. Polling is unaffected;
  this gates uploads only.
- **Prometheus metrics endpoint (Phase 5, item 2)**: optional
  `GET /metrics` HTTP endpoint exposing per-mount cache usage, queue
  depth, hydration counts, conflict count, and poller state. Opt-in via
  `[daemon.metrics] enabled = true` (default off, default bind
  `127.0.0.1:9090`). Hand-rolled HTTP/1.1 listener on `tokio::net`
  rather than pulling in axum/hyper-server for a single endpoint —
  reuses the existing `DaemonState::snapshot()` aggregation.
- **Selective sync (Phase 5, item 1)**: per-mount `ignore_patterns`
  config field filters glob-matched paths out of indexing, FUSE
  `create`/`mkdir`, and upload events. Patterns use `globset` semantics
  (`*` matches across `/`, so `*.log` catches log files at any depth;
  use `node_modules/**` for subtrees). Bad patterns fail the daemon at
  startup with a clear error. Already-indexed entries that match a
  newly-added pattern are preserved — rules prevent new indexing, never
  retroactively unindex. Ignored FUSE creates return `EPERM` so apps see
  a visible failure rather than silently writing into an untracked file.

### Changed
- **Conflict files isolated under `.stratosync-conflicts/`**: conflict
  siblings are stored on the remote under a dedicated prefix instead of
  alongside their canonical files, keeping the user-visible namespace
  clean. Poller, watcher, and upload queue filter the conflict prefix
  on every ingress so files don't get re-imported or re-uploaded as
  regular content. Added a startup tree walk to migrate any
  pre-existing conflict files into the new namespace.
- **Poller runs immediately on startup** instead of sleeping the first
  poll interval, so directory listings populate without latency on
  first navigation. Directories are also bulk-marked as `dir_listed`
  after a full recursive poll, eliminating per-directory backend
  `list()` calls during traversal.

### Fixed
- Conflict sibling lookup used the wrong column name (`parent` vs
  `parent_inode`), causing false negatives in the conflicts cleanup
  walker.
- Deadlock and orphan handling in conflicts cleanup when a sibling's
  parent had already been removed.
- `WebDavSidecar` carried an unused `port` field; removed.

## [0.11.0] - 2026-04-11

### Added
- **WebDAV sidecar backend**: Optional `rclone serve webdav` subprocess per mount
  for low-latency HTTP-based transfers instead of spawning rclone per operation.
  Enable with `[daemon] webdav_sidecar = true`. Implements full Backend trait
  via HTTP/WebDAV protocol (GET, PUT, PROPFIND, MKCOL, DELETE, MOVE).
- **Nautilus file manager extension**: Python GObject extension reads
  `user.stratosync.status` xattr to show sync status emblem overlays
  (checkmark for cached, sync arrows for uploading, warning for conflicts).
  Supports Nautilus 3.0 and 4.0.
- **System tray indicator** (`stratosync-tray`): New workspace crate using
  `ksni` (StatusNotifierItem). Polls mount databases every 5s, shows per-mount
  cache usage, syncing count, conflicts, and pinned files. Autostart desktop
  file included.
- **Distribution packaging templates**: Debian `.deb` (via debian/), Fedora
  `.rpm` (spec file), and Arch AUR (PKGBUILD). All include binaries, systemd
  unit, and Nautilus extension.
- `RcloneBackend::which_rclone()` public method for locating the rclone binary.

### Changed
- **Phase 4 complete**: all planned Phase 4 deliverables are now implemented.

## [0.10.0] - 2026-04-11

### Added
- **Pin/unpin for offline availability**: `stratosync pin <path>` downloads and pins
  files so they survive cache eviction. Supports recursive directory pinning.
  `stratosync unpin <path>` releases the pin. `stratosync status` shows pinned count.
- **Background hydration with range-read fast path**: `read()` on a file that's still
  downloading now uses `rclone cat --offset/--count` to serve the requested bytes
  immediately while the full download continues in the background. Falls back to
  blocking wait for backends that don't support range requests.
- **`download_range()` Backend trait method**: New `download_range(remote, offset, len)`
  with default `NotSupported` fallback. Implemented for RcloneBackend and MockBackend.
- **`SyncError::NotSupported` variant** for graceful feature detection.
- **Readdir small-file prefetch**: When a directory is listed for the first time,
  files under `prefetch_threshold` (default 1 MB) are hydrated in the background.
  Configurable via `[daemon.sync] prefetch_threshold = "1 MB"` (set to "0" to disable).
- **StateDb pinning methods**: `set_pinned()`, `is_pinned()`, `pinned_count()`,
  `list_file_descendants()`.

## [0.9.0] - 2026-04-11

### Added
- **`conflicts resolve` CLI**: four subcommands for resolving conflict files:
  - `stratosync conflicts keep-local <path>` — upload local version, discard remote conflict
  - `stratosync conflicts keep-remote <path>` — download remote version, discard local
  - `stratosync conflicts merge <path>` — attempt 3-way merge using base version store
  - `stratosync conflicts diff <path>` — show unified diff between local and remote
- **Desktop notifications for upload failures**: `notify-send` alerts when uploads
  fail fatally, in addition to existing conflict notifications.
- **xattr sync status**: read-only extended attributes on every FUSE-mounted file:
  - `user.stratosync.status` — current sync state (remote, cached, dirty, uploading, conflict)
  - `user.stratosync.etag` — remote version identifier
  - `user.stratosync.remote_path` — path on the remote backend
  - `setxattr`/`removexattr` return `ENOTSUP` (read-only).
- **`stratosync-core::merge` module**: extracted `MergeOutcome`, `try_three_way_merge()`,
  and `git_available()` from the daemon into core for CLI reuse.
- **Shared notification module** (`sync::notification`): extracted from conflict.rs
  for reuse across upload queue and conflict resolver.
- AWS S3 setup script (`scripts/setup-aws-s3.sh`) for bucket, IAM, rclone,
  and stratosync config in one step.

### Changed
- **Phase 3 complete**: all planned Phase 3 deliverables are now implemented.

## [0.8.0] - 2026-04-10

### Added
- **3-way text merge for conflict resolution**: When `text_conflict_strategy = "merge"`
  is set, concurrent edits to text files are resolved automatically via
  `git merge-file`. Non-overlapping changes merge cleanly with no conflict
  file; overlapping changes produce conflict markers. Falls back to keep-both
  for binary files, when git is unavailable, or when no base version exists.
- **Base version object store** (`BaseStore`): Content-addressed SHA-256 store
  at `{cache_dir}/.bases/objects/`. Base versions are captured after each
  successful upload and used as the common ancestor for 3-way merge. Includes
  automatic eviction of stale bases (configurable retention, default 30 days).
- **`SyncConfig` merge settings**: `text_conflict_strategy` (keep_both/merge),
  `base_retention_days`, `base_max_file_size`, `text_extensions` allowlist.
- **`base_versions` DB table** with reference counting for deduplication.
- 6 end-to-end conflict resolution tests (clean merge, conflict markers,
  no-base fallback, binary skip, strategy toggle, no-git fallback).
- **Desktop notifications** on conflict via `notify-send`.

### Fixed
- **ETag conflict detection used file IDs, not content hashes**: `stat()` was
  called without `--hash`, so rclone returned no content hashes. Google Drive
  file IDs don't change on content update, making conflicts undetectable.
  Now `stat()` always requests hashes.
- **Hash key case mismatch**: rclone outputs lowercase hash names (`sha1`,
  `md5`) but the code looked for uppercase (`SHA-1`, `MD5`). Now checks both.
- **Upload queue race condition**: The abort-and-respawn debounce mechanism
  had multiple race conditions causing uploads to be silently dropped.
  Redesigned to deadline-based debounce (no oneshot channels, no abort
  mechanism).
- **Watcher inode mismatch after poller upsert**: The inotify watcher looked
  up inodes via `cache_path` column, which is NULL for entries replaced by
  the poller. Now derives `remote_path` from the cache file path and uses
  `get_by_remote_path()`.
- **Delta poller proceeds when initial listing fails**: Instead of blocking
  delta mode, logs a warning and enters delta-only mode.

### Changed
- **Upload queue**: Complete rewrite of debounce mechanism from abort-and-
  respawn (oneshot channels + JoinSet) to deadline-based (HashMap of
  `inode -> due_at`). Eliminates all race conditions.
- **`RcloneBackend::stat()`**: Now includes `--hash` flag for content-based
  change detection.

## [0.7.2] - 2026-04-10

### Fixed
- **Google Drive 403 rate limits**: Google returns quota errors as HTTP 403
  with `rateLimitExceeded` in the body, not 429. The error mapper now checks
  the body for rate-limit keywords before falling through to PermissionDenied,
  so the poller correctly backs off instead of halting.
- **OAuth token refresh via rclone**: Token refresh now always delegates to
  rclone (`rclone about` to trigger refresh, then `rclone config show` to
  read the fresh token). The previous direct-API refresh failed when rclone
  used its built-in shared credentials (client_id not in config).
- **Stale token after expiry**: `rclone config show` only reads the config
  file — it does not trigger an OAuth refresh. Added a two-step process:
  `rclone about <remote>:` forces authentication (refreshing the token),
  then `rclone config show` reads the now-fresh token.
- **Redundant full listings on rate limit**: When the initial `get_start_token`
  failed after a successful full listing, the token was never stored, causing
  every retry to redo the full listing. Now the poller detects existing DB
  entries and skips the listing, only retrying the token acquisition.
- **401 retry with force-refresh**: `changes_since` and `start_token` now
  catch HTTP 401, force-refresh the OAuth token via rclone, and retry once
  before failing. Handles the race where the token expires between the
  expiry check and the actual API call.

## [0.7.1] - 2026-04-10

### Added
- **OneDrive delta polling**: The remote poller now supports OneDrive's delta
  API (`deltaLink`) for incremental change detection, using Microsoft Graph.
  Enabled automatically when the rclone remote type is `onedrive`. OneDrive's
  delta API returns full paths (via `parentReference.path`), making path
  resolution simpler than Google Drive's ID-based approach.
- **`OneDriveDelta` implementation**: Full `DeltaProvider` implementation with
  OAuth token refresh via Microsoft's token endpoint, pagination support,
  `token=latest` for start tokens, and `resyncRequired` error handling.
- 17 new OneDrive-specific unit tests (JSON parsing, path resolution, error
  mapping).

### Changed
- **`RcloneBackend::init_delta`**: Now constructs `OneDriveDelta` for
  `type = onedrive` remotes (previously logged a warning and fell back to
  full listing). Supports `drive_id` config for business/shared drives.

## [0.7.0] - 2026-04-10

### Added
- **Google Drive delta polling**: The remote poller can now use Google Drive's
  Changes API (`pageToken`) for incremental change detection instead of full
  recursive listings every poll cycle. This dramatically reduces API calls and
  latency for large mounts. Enabled automatically when the rclone remote type
  is `drive`.
- **`DeltaProvider` trait**: New extensible trait in `stratosync-core` for
  provider-specific delta APIs. `GoogleDriveDelta` is fully implemented;
  `OneDriveDelta` is stubbed for future implementation.
- **`SyncError::TokenExpired`** variant for change token invalidation (HTTP 410).
  The poller automatically falls back to a full listing and obtains a fresh
  token when this occurs.
- **rclone config parser**: Reads OAuth credentials from `rclone config show`
  output, handling encrypted configs and rclone's built-in credentials.
- **`StateDb` change token methods**: `get/set/clear_change_token()` for opaque
  delta token storage, and `delete_remote_entry_by_path()` for individual
  entry deletion (used by delta polling).
- 10 new delta integration tests, 18 unit tests for the delta module, 11 for
  the rclone config parser, 7 for the StateDb additions.
- **OneDrive delta stub**: Provider detection for `onedrive:` remotes is in
  place; implementation will follow in a future release.

### Changed
- **`Backend` trait**: Added `get_start_token()` method (with default impl)
  for obtaining initial change tokens.
- **`RcloneBackend`**: Now holds an optional `DeltaProvider` initialized at
  startup via `init_delta()`. `supports_delta()` and `changes_since()` delegate
  to the provider when available.
- **`MockBackend`**: Extended with `enable_delta()`, `push_change()`, and
  `set_delta_error()` for testing delta workflows.

### Known Limitations
- Google Drive shared drives are not supported (requires `driveId` parameter).
- Path resolution for deeply nested files may require additional API calls for
  parent chain resolution (mitigated by in-memory cache per poll cycle).

## [0.6.1] - 2026-04-09

### Fixed
- **mkdir + immediate file create**: Creating a file inside a newly-made
  directory failed with EIO because `populate_directory` tried to list the
  directory on the remote before the async `mkdir` had completed. Now, if
  the backend listing fails for a locally-created directory (status is not
  `Remote`/`Stale`), the directory is treated as empty and marked listed so
  that `create`/`lookup` inside it can proceed immediately.

## [0.6.0] - 2026-04-09

### Fixed
- **Multi-mount isolation bug**: Two mounts with different rclone remotes showed
  identical content. Root cause: all mounts shared a single `state.db`, causing
  root inode collisions (`INSERT OR REPLACE` at inode 1) and unfiltered
  `list_children`/`get_by_parent_name` queries that returned cross-mount entries.

### Changed
- **Per-mount database files**: Each mount now gets its own SQLite database
  (`{name}.db`) instead of sharing `state.db`. This provides complete isolation
  of inode namespaces and file entries between mounts.
- **`StateDb::list_children` and `get_by_parent_name`** now require a `mount_id`
  parameter for defense-in-depth filtering (breaking API change for downstream
  consumers of `stratosync-core`).
- CLI commands (`status`, `conflicts`) open per-mount database files.

### Migration
- The old shared `state.db` is no longer used. The daemon will create new
  per-mount databases on first run. Cached files from the old DB will need to
  be re-hydrated.

## [0.5.2] - 2026-04-09

### Fixed
- **Systemd service: remove PrivateTmp and NoNewPrivileges** — `PrivateTmp=true`
  creates a private mount namespace that makes the FUSE mount invisible outside
  the daemon's process. `NoNewPrivileges=true` blocks the setuid `fusermount3`
  binary from elevating privileges, causing "Operation not permitted" on mount.
  Both directives have been removed from the service template with explanatory
  comments documenting why they are incompatible with FUSE.

## [0.5.1] - 2026-04-07

### Fixed
- **Hydration timeout**: `read()` no longer hangs forever if a download dies.
  Waits up to 5 minutes with 3 retries, then returns EAGAIN (retry later).
- **Errno mapping**: Network errors → EHOSTUNREACH ("No route to host"),
  transient errors → EAGAIN ("Resource temporarily unavailable"), conflicts →
  EEXIST, disk full → ENOSPC. Users now see "No space left on device" instead
  of generic "Input/output error" for quota/disk issues.
- **Rclone error parsing**: Extracts human-readable messages from rclone's JSON
  log format. Auth errors (invalid_grant, token expired/revoked) map to
  "Permission denied". Timeout/DNS → "Host unreachable".
- **Poller backoff**: Exponential backoff after 3 consecutive failures (doubles
  interval, caps at 10 min). After 10 failures, logs ERROR with actionable
  guidance instead of spamming identical warnings.
- **Upload queue resilience**: On loop exit, resets stuck Uploading inodes to
  Dirty and logs ERROR. Previously, writes silently stopped syncing.
- **Rclone process cleanup**: `kill_on_drop` ensures timed-out rclone processes
  are killed (not orphaned). 256MB output cap prevents OOM from huge listings.
- **Batch chunking**: DB operations process in chunks of 1000, releasing the
  mutex between chunks so FUSE operations aren't starved during large polls.
- **QuotaExceeded is now retryable** with exponential backoff.

## [0.5.0] - 2026-04-07

### Performance
- **Async mkdir and rename**: Both now return instantly (<1ms). Remote operations
  run in background tasks. Previously 200-1000ms per call.
- **Deferred hydration**: `open()` returns immediately for uncached files. The
  download starts in the background; `read()` blocks only if data isn't ready yet.
  Enables parallel downloads when opening multiple files.
- **Prefetch child directories**: When a directory is first listed, its
  subdirectories are populated in the background (4 concurrent). Next `cd`
  into a child is instant.
- **Stop invalidating dir_listed on local writes**: `touch file; ls` no longer
  re-fetches the directory from rclone. Local writes update the DB directly;
  the poller handles remote changes independently.
- **Increase attr/entry timeout**: 5s → 60s. Kernel caches directory entries
  and file attributes 12x longer, reducing FUSE→daemon queries.

## [0.4.0] - 2026-04-06

### Added
- **ETag-based poll diffing**: The poller now diffs the remote listing against a
  DB snapshot in memory. Only entries that actually changed (new, modified, or
  deleted) trigger DB writes. Idle polls with 2793 files go from 2793 upserts
  to 1 SELECT + 1 batch UPDATE.
- **Remote deletion detection**: Entries absent from the remote listing are
  automatically removed from the DB (unless dirty/uploading). Previously,
  remote deletions were invisible to the daemon.
- `--hash` flag on `rclone lsjson --recursive` for content-based change
  detection via ETags (MD5/SHA-1).
- New migration `0003_poll_generation` adding generation counter to file_index.
- `StateDb::snapshot_remote_index`, `batch_mark_generation`,
  `delete_stale_entries`, `get/set_poll_generation`.
- `MockBackend::remove_file()` and `modify_file()` for simulating remote changes.
- 10 new tests for poll diffing behavior.

## [0.3.0] - 2026-04-04

### Added
- **Delete tombstones**: `rm` and `rm -rf` now work reliably. When files are
  deleted locally, a tombstone record prevents the poller from re-adding them
  before the background remote delete completes. Directory tombstones also
  block children (prefix match). Tombstones expire after 5 minutes as a safety
  net and are cleaned up each poll cycle.
- New migration `0002_delete_tombstones` with `delete_tombstones` table
- 9 new tombstone tests

## [0.2.1] - 2026-04-03

### Security
- **Path traversal**: `validate_filename()` rejects `..`, `/`, null bytes in all
  FUSE create/mkdir/rename handlers. `safe_cache_path()` canonicalizes paths and
  verifies they stay within the cache directory.
- **Symlink attacks**: Upload queue rejects symlink cache files via
  `symlink_metadata()`. `.meta/partial` directory set to 0o700. Hydration temp
  files use random suffixes to defeat symlink races.
- **DoS protection**: Poller rejects remote listings exceeding 500,000 entries.
- **Input sanitization**: `populate_directory` and poller skip entries with `..`
  or null bytes in filenames from remote listings.

### Added
- 5 security regression tests (path traversal, symlink detection, SQL injection,
  integer boundaries)

## [0.2.0] - 2026-04-03

First functional release. The daemon mounts a Google Drive (or any rclone remote)
as a FUSE filesystem with bidirectional sync.

### Added
- `setattr` support for file truncation (`>` redirection on existing files)
- `Backend::rmdir()` trait method using `rclone rmdir` for directory removal
- `StateDb::insert_root()` for reliable root inode creation at inode 1
- `StateDb::batch_upsert_remote_files()` for transactional directory population
- `StateDb::set_dirty_size()` to track file size after writes
- `StateDb::delete_mount_entries()` for clearing corrupt state on startup
- Non-blocking `unlink`/`rmdir` — remote deletes run in background tokio tasks
- Clean shutdown via `fusermount3 -u` on ctrl-c
- Remote poller resolves parent inodes from path hierarchy (not all files at root)
- 29 functional tests covering multi-component workflows
- CLAUDE.md with build commands and architecture overview

### Fixed
- **FUSE thread panic**: `Handle::current()` called from a `std::thread` with no
  tokio runtime context. Now the Handle is captured in the main thread and passed in.
- **readdir EIO**: `reply.ok()` was not called before returning when the FUSE reply
  buffer was full. Fuser's `Drop` impl sent `-EIO` for unsent replies.
- **Files read as empty**: `handle_write` updated the cache file but never updated
  the file size in the DB. `getattr` reported size 0, so the kernel never read data.
- **Duplicate directory entries**: `join_remote("/", name)` produced `/name` but
  rclone returns `name` (no leading slash). Paths now consistently omit leading slashes.
- **Wrong delete paths**: `populate_directory` stored rclone's relative paths instead
  of full paths from root. Deletes targeted wrong remote locations.
- **Rename lost file content**: `rename_entry` didn't update `cache_path` in the DB.
  After rename, `open()` tried the old (missing) cache path.
- **Rename failed on new files**: `rclone moveto` was called on Dirty files that
  hadn't been uploaded yet. Now only remotely-existing files trigger remote renames.
- **rmdir used wrong rclone command**: `deletefile` (for files) was used instead of
  `rmdir` (for directories).
- **Daemon hung on shutdown**: `fuser::mount2` blocks until unmount, but nothing
  triggered unmount on ctrl-c.
- **inotify watcher died immediately**: `FsWatcher` was dropped right after creation
  because the return value wasn't stored.
- **Root inode corruption**: Self-referencing FK (`parent_inode=1` on inode 1) failed
  when autoincrement advanced past 1. Root now uses explicit inode=1 with NULL parent.
- **Startup race**: Poller started before root inode existed, causing FK violations.
  Root creation now happens before any background tasks start.
- **Cache/mount dirs missing at startup**: Watcher and FUSE mount failed because
  directories weren't created until later in the startup sequence.
- **NULL parent_inode crash**: `row_to_entry` read parent as non-nullable after root
  was changed to NULL parent.
- **Silent error swallowing**: ~20 instances of `let _ = result` across the daemon
  replaced with `warn!` logging. Two intentional cases (oneshot senders) documented.
- **Poller flattened hierarchy**: All 2793 files were inserted with `parent=root`.
  Now resolves parent inodes from path components.
- **rclone "doesn't exist" mapped to Fatal**: Stderr containing "doesn't exist" or
  "404" now correctly maps to `NotFound`.

### Changed
- README Quick Start updated to match actual CLI commands
- `handle_unlink`/`handle_rmdir` delete DB entries immediately, queue remote
  deletes asynchronously
- `populate_directory` uses `batch_upsert_remote_files` (single transaction)
  instead of individual upserts

## [0.1.0] - 2026-03-30

### Added
- Initial implementation of Phase 1 (read-only VFS) and Phase 2 (bidirectional sync)
- FUSE filesystem: lookup, getattr, readdir, open, read, write, create, mkdir,
  unlink, rmdir, rename, fsync
- SQLite state database with WAL mode, migrations, file index, sync queue, cache LRU
- rclone backend: lsjson, copyto, deletefile, mkdir, moveto, about
- Upload queue with debounce and concurrency control
- Remote poller (polling-based change detection)
- Cache manager with LRU eviction and configurable quota
- inotify watcher for local change detection
- Conflict resolver with `.conflict.{ts}.{hash}` naming
- CLI: status, ls, config (show/test/edit), conflicts, pin/unpin (stubs)
- Systemd user service unit
- Install script with prerequisite checks
