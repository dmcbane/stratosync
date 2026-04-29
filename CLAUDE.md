# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

Stratosync is a Linux cloud sync daemon providing on-demand virtual filesystem via FUSE3, with multi-backend support through rclone. Files appear immediately with metadata-only placeholders, hydrate on `open()`, and uploads propagate automatically with conflict detection.

**Status**: Beta (v0.12.0-beta.1). Phases 1–6 functionally complete; encrypted caching is the only Phase 5 item deferred (to v0.13.0+). v0.11.0 was the last alpha release. v0.12.0-beta.1 adds: dashboard TUI (`stratosync dashboard`), file versioning (`stratosync versions`), selective sync via per-mount `ignore_patterns`, bandwidth scheduling, Prometheus `/metrics`, multi-FM integration (Nemo / Caja extensions and Dolphin emblem-overlay plugin alongside the existing Nautilus extension; KDE/Thunar/PCManFM context-menu actions), conflicts cleanup CLI, conflict-namespace isolation under `.stratosync-conflicts/`, and the `stratosync daemon` subcommand wrapping `systemctl --user` / `journalctl`.

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
cargo run -p stratosync-cli -- pin <path>            # download + lock for offline use
cargo run -p stratosync-cli -- unpin <path>          # release the offline lock
cargo run -p stratosync-cli -- daemon status         # systemctl/journalctl wrapper
cargo run -p stratosync-cli -- daemon logs --follow
```
The CLI binary is `stratosync`. There is no `init` subcommand. `daemon` *is* a real subcommand (added in v0.12.0-beta.1) — it's a thin wrapper over `systemctl --user` / `journalctl` for the `stratosyncd.service` user unit, not a way to run the daemon in-process.

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

Project MSRV is **Rust 1.80** (declared in `install.sh` and the Prerequisites
section above). CI uses `dtolnay/rust-toolchain@stable`, which floats well
above the MSRV; the pins below exist to keep the build working *for users*
on a 1.80 toolchain, not to keep CI happy.

- `clap = "=4.5.57"` — 4.6+ requires `edition2024` (Rust 1.85)
- `toml = "=0.7.2"`, `toml_edit = "=0.19.9"` — later versions pull `indexmap` 2.12+ which needs Rust 1.82
- `fuser = "0.14"` — 0.15+ adds `clap` as a non-dev dep, which collides with our `clap` pin's exact-version resolution. Not MSRV-driven, but unpinning needs the `clap` pin lifted first.

Bumping MSRV to 1.85 (planned for v0.13.0+) lifts every constraint above and
the pins can come out together.
