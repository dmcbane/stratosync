# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

Stratosync is a Linux cloud sync daemon providing on-demand virtual filesystem via FUSE3, with multi-backend support through rclone. Files appear immediately with metadata-only placeholders, hydrate on `open()`, and uploads propagate automatically with conflict detection.

**Status**: Beta (v0.13.0-beta.6). Phases 1–6 functionally complete; encrypted caching is the only Phase 5 item deferred. v0.11.0 was the last alpha release. v0.12.0 added: dashboard TUI (`stratosync dashboard`), file versioning (`stratosync versions`), selective sync via per-mount `ignore_patterns`, bandwidth scheduling, Prometheus `/metrics`, multi-FM integration (Nemo / Caja extensions and Dolphin emblem-overlay plugin alongside the existing Nautilus extension; KDE/Thunar/PCManFM context-menu actions), conflicts cleanup CLI, conflict-namespace isolation under `.stratosync-conflicts/`, and the `stratosync daemon` subcommand wrapping `systemctl --user` / `journalctl`. v0.12.1 fixes Dolphin/Nautilus interop: `statfs` reports real host-fs totals, copy-overwrite no longer races the upload-finalizer into clobbering `cache_path`, and directory mutations invalidate the kernel readdir cache. v0.12.2 surfaces hydration health to the tray/dashboard/Prometheus via a new `mount_health` table. v0.12.3 self-heals stale `remote_path` rows left by OneDrive delta missing renames. v0.12.4 (v0.13 milestone 1) gives every row a backend-stable `remote_item_id` and routes the poller through `upsert_remote_file_by_id_or_path` so OneDrive renames update in place. **v0.13.0-beta.1 (milestone 2)** bumps MSRV to Rust 1.85 (all dep pins lifted), and the v0.12.3 self-heal now lazily backfills `remote_item_id` from the verifying `stat()` so pre-v0.13 rows graduate into the id-aware path on first failure. **v0.13.0-beta.2 (milestone 3)** extends `RemoteChange::Deleted` with `item_id` so renamed-then-deleted-in-the-same-delta-page items still resolve cleanly, and enriches the self-heal log to distinguish legacy null-id pruning (expected, tapers) from known-id pruning (unexpected — investigate). **v0.13.0-beta.3** adds `stratosync cache clear [--mount NAME | --all]` — an operator escape hatch that drops hydrated cache files and reverts rows from `cached`/`stale` to `remote` so the next access re-hydrates from the cloud; dirty/uploading/conflict/pinned rows are preserved by default. Refuses to run while the daemon is up (probes the IPC socket) unless `--force` is passed. **v0.13.0-beta.4** hardens the OneDrive delta-channel parser: items with unrecognized `parentReference.path` shapes are now skipped (with an enriched diagnostic warn) instead of being passed through with the raw string as their parent path — same class of bug as the SharePoint duplicate-folder regression, but for shapes Graph might add in the future or that we haven't discovered yet. **v0.13.0-beta.5** enriches the dashboard in-flight panel with attempt count, first-seen time (preserved across retries), and live byte progress parsed from `rclone --stats=1s` output. New `Backend::upload_with_progress` with default fallback; `RcloneBackend` streams stderr line-by-line via the new `run_with_progress` helper. Resolves the "timer fluctuates 0..60s" mystery (retry loop) by making it visible in the UI. **v0.13.0-beta.6** does the symmetric work for the *download* side: new `Backend::download_with_progress`, a `HydrationTracker` (three `Arc<DashMap>`s) shared between `do_hydrate` and the dashboard snapshot, an `ActiveHydration` IPC row, and a hydrations sub-panel in the in-flight view. Triggered by user-reported "copy from OneDrive: 14/20 succeeded then froze" — without per-file download visibility, a single stalled `cp` looked indistinguishable from a wedged daemon.

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
cargo run -p stratosync-cli -- cache clear --mount NAME   # drop hydrated files; force re-hydrate
cargo run -p stratosync-cli -- cache clear --all          # same, every enabled mount
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

- Rust 1.85+
- `libfuse3-dev` (`sudo apt install libfuse3-dev` on Debian/Ubuntu)
- `rclone` for runtime backend access (not needed for tests)

## Dependency Versions

Project MSRV is **Rust 1.85** (declared in `install.sh` and the
Prerequisites section above). v0.12.5 lifted the v0.12.x pins (`clap`,
`toml`, `toml_edit`, `fuser`) when the MSRV bumped — there are no
exact-version pins left. Add new dependencies at their latest minor
release; bump MSRV explicitly if a new dep requires a newer toolchain.
