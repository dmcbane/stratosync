# Changelog

All notable changes to this project will be documented in this file.

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
