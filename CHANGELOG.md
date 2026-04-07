# Changelog

All notable changes to this project will be documented in this file.

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
