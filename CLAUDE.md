# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

Stratosync is a Linux cloud sync daemon providing on-demand virtual filesystem via FUSE3, with multi-backend support through rclone. Files appear immediately with metadata-only placeholders, hydrate on `open()`, and uploads propagate automatically with conflict detection.

**Status**: Pre-alpha (v0.6.1). Phase 1 (read-only VFS) and Phase 2 (bidirectional sync) are implemented and tested with Google Drive. Phase 3+ (delta APIs, desktop integration, selective sync) are not started.

## Build & Test Commands

```bash
cargo build                                       # debug build
cargo build --release                             # release build
cargo test --workspace                            # all tests
cargo test -p stratosync-core                     # core unit tests only
cargo test -p stratosync-core --test integration  # integration tests with mock backend
cargo test -p stratosync-core -- test_name        # single test by name
```

Tests use in-memory SQLite and `MockBackend` — no cloud credentials or FUSE module needed.

**Run daemon locally** (foreground, debug logging):
```bash
RUST_LOG=stratosync=debug cargo run -p stratosync-daemon
```
The daemon binary is `stratosyncd` (not a subcommand of `stratosync`). It reads config from `~/.config/stratosync/config.toml` (override with `STRATOSYNC_CONFIG` env var). Requires at least one enabled `[[mount]]` in config.

**Run CLI**:
```bash
cargo run -p stratosync-cli -- status              # sync status across mounts
cargo run -p stratosync-cli -- ls [path]            # list remote contents
cargo run -p stratosync-cli -- config show|test|edit
cargo run -p stratosync-cli -- conflicts            # list conflict files
cargo run -p stratosync-cli -- pin <path>            # (stub) pin for offline
cargo run -p stratosync-cli -- unpin <path>          # (stub) remove pin
```
The CLI binary is `stratosync`. There is no `init` or `daemon` subcommand — the README's old Quick Start was wrong.

## Architecture

**Workspace crates** (three-crate split is a key design constraint):

- **`stratosync-core`** — Pure types, `Backend` trait, `StateDb` (SQLite), config structs. No FUSE, no `notify`, no `toml` dependency. Fast to compile, easy to unit test.
- **`stratosync-daemon`** (`stratosyncd` binary) — FUSE filesystem, sync engine, cache manager, inotify watcher. Uses `tokio` runtime; FUSE callbacks bridge to async via `Handle::block_on()`.
- **`stratosync-cli`** (`stratosync` binary) — User-facing CLI via `clap`. Talks to the state DB and config files.

**Key patterns**:

- **Hydration model**: VFS shows all files immediately with remote metadata. `open()` blocks until the file downloads to local cache. Concurrent `open()` calls for the same inode coalesce via `DashMap<Inode, Vec<oneshot::Sender>>`.
- **Upload model**: Writes go to local cache immediately. `UploadQueue` debounces per-inode (2s default), `fsync()` bypasses the debounce.
- **Conflict resolution**: Optimistic lock via ETag. Conflicts produce `.conflict.{ts}.{hash}` sibling files.
- **Crash safety**: WAL-mode SQLite with `sync_queue` table; startup recovery replays incomplete operations.
- **Cache eviction**: LRU with configurable quota via `CacheManager`.

## Prerequisites

- Rust 1.80+
- `libfuse3-dev` (`sudo apt install libfuse3-dev` on Debian/Ubuntu)
- `rclone` for runtime backend access (not needed for tests)

## Dependency Pinning

Several deps are pinned for Rust 1.75 CI compatibility:
- `clap = "=4.5.57"` (4.6+ needs edition2024)
- `toml = "=0.7.2"`, `toml_edit = "=0.19.9"` (later versions pull indexmap 2.12+ → Rust 1.82)
- `fuser = "0.14"` (0.15+ pulls clap as non-dev dep)

Remove all pins when CI moves to Rust 1.85+.
