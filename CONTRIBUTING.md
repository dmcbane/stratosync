# Contributing to stratosync

## Prerequisites

- **Rust 1.80+** — `rustup update stable`
- **rclone** — `https://rclone.org/install/`
- **libfuse3-dev** — `sudo apt install libfuse3-dev` (Debian/Ubuntu)
- **FUSE kernel module** — `sudo modprobe fuse` (usually already loaded)

## Building

```bash
git clone https://github.com/dmcbane/stratosync
cd stratosync
cargo build                   # debug
cargo build --release         # release
```

## Testing

```bash
cargo test --workspace                        # all tests (no credentials needed)
cargo test -p stratosync-core                 # core unit tests only
cargo test -p stratosync-core --test integration  # integration tests with mock backend
```

Tests use an in-memory SQLite DB and `MockBackend` — no cloud credentials or FUSE module required.

## Running locally

```bash
# 1. Configure a cloud remote
rclone config

# 2. Create config
mkdir -p ~/.config/stratosync
cat > ~/.config/stratosync/config.toml << 'TOML'
[[mount]]
name          = "gdrive"
remote        = "gdrive:/"
mount_path    = "~/GoogleDrive"
cache_quota   = "5 GiB"
poll_interval = "60s"
TOML

# 3. Test connectivity
cargo run -p stratosync-cli -- config test

# 4. Start daemon (in foreground for development)
RUST_LOG=stratosync=debug cargo run -p stratosync-daemon

# 5. In another terminal:
ls ~/GoogleDrive/
```

## Code layout

```
crates/
  stratosync-core/     Core types, SQLite state DB, rclone backend, config structs
    src/
      backend/         Backend trait + RcloneBackend + MockBackend
      config/          Config types (serde only, no file I/O)
      state/           StateDb (SQLite), NewFileEntry, migrations
      types/           FileEntry, SyncStatus, SyncError, RemoteMetadata
    tests/
      integration.rs   25+ integration tests

  stratosync-daemon/   FUSE daemon (stratosyncd)
    src/
      fuse/            Filesystem impl (lookup/getattr/readdir/open/read/write/create/…)
      sync/            UploadQueue, RemotePoller, ConflictResolver
      cache/           CacheManager (LRU eviction)
      watcher/         inotify → UploadQueue bridge
      config_io.rs     Config file loading (toml crate)
      main.rs          Startup, signal handling, per-mount orchestration

  stratosync-cli/      CLI (stratosync)
    src/
      commands/        status, ls, config, conflicts
      config_io.rs     Config file loading
      main.rs          clap CLI definition

contrib/
  systemd/             stratosyncd.service user unit
docs/
  architecture/        Design docs for each subsystem
  ROADMAP.md           5-phase implementation plan
install.sh             Build + install script
setup_repo.sh          GitHub repo creation script
```

## Architecture notes

The key design decision is the **two-layer architecture**:

1. `stratosync-core` — no `toml` dep, no FUSE dep, no `notify` dep. Pure types + DB + backend trait. This makes it fast to compile and easy to test without any system deps.

2. `stratosync-daemon` — brings in `fuser`, `notify`, `tokio` runtime. The `Filesystem` impl calls async DB/backend operations by parking on `tokio::Handle::block_on()` since FUSE callbacks are synchronous.

The **hydration model**: files appear immediately in the VFS with correct metadata. `open()` blocks the calling process until the file is downloaded to local cache. Multiple concurrent `open()` calls for the same inode are coalesced via per-inode waiters (`DashMap<Inode, Vec<oneshot::Sender>>`).

The **upload model**: writes go to the local cache file immediately. A debounced `UploadQueue` coalesces writes per inode and uploads after the debounce window expires (default 2s). `fsync()` bypasses the debounce.

## Phase status

- [x] Phase 1: Read-only VFS (lookup, getattr, readdir, open, read)
- [x] Phase 2: Bidirectional sync (write, create, mkdir, unlink, rmdir, rename, fsync, upload queue, conflict resolver, LRU cache, inotify watcher, remote poller)
- [x] Phase 3: Conflict resolution & safety (3-way merge, delta APIs, conflicts CLI, desktop notifications, xattr sync status)
- [x] Phase 4: Performance & UX (pin/unpin, range hydration, prefetch, WebDAV sidecar, Nautilus extension, tray indicator, packaging)
- [ ] Phase 5: Selective sync, bandwidth scheduling, Prometheus metrics

## Dependency version notes

This project deliberately pins some versions for Rust 1.75 compatibility in CI:

```toml
clap     = "=4.5.57"   # 4.6+ pulls clap_lex 1.x which requires edition2024
toml     = "=0.7.2"    # 0.8+ pulls toml_edit 0.21+ → indexmap 2.12+ → Rust 1.82
toml_edit = "=0.19.9"  # forces indexmap 1.x, compatible with Rust 1.63+
fuser    = "0.14"      # 0.15+ pulls clap as non-dev dep → same chain as above
```

On Rust 1.85+ (current stable), remove all pins and use latest versions.
