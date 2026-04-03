# stratosync

**A first-class Linux cloud sync daemon** — on-demand virtual filesystem, multi-backend, conflict-aware.

Stratosync brings the OneDrive/Google Drive experience to Linux: files appear immediately in your filesystem, data is fetched on-demand, changes upload automatically, and conflicts are preserved rather than silently destroyed.

```
~/Cloud/                     ← FUSE3 virtual mount
├── Documents/
│   ├── report.pdf           ← hydrated (local cache)
│   └── draft.docx           ← placeholder (metadata only, fetched on open)
└── Photos/
    └── 2025/                ← directory listing from remote index
```

## Status

🚧 **Pre-alpha (v0.2.0)** — Phase 1 (read-only VFS) and Phase 2 (bidirectional sync) are implemented and tested with Google Drive. The daemon mounts, lists, reads, writes, renames, and deletes files. Conflict resolution, LRU cache eviction, and inotify-based change detection are functional.

## Design Goals

| Goal | Approach |
|------|----------|
| On-demand file hydration | FUSE3 `open()` hook triggers rclone download |
| Multi-backend | rclone subprocess/sidecar abstracts 70+ providers |
| Conflict safety | Optimistic-lock via ETag; `.conflict` siblings on collision |
| Crash safety | Write-ahead log in SQLite; atomic rename on finalize |
| Low idle overhead | inotify for local changes; polling or provider delta API for remote |
| Storage pressure | LRU eviction from local cache with configurable quota |

## Supported Backends (via rclone)

Google Drive · OneDrive · Dropbox · S3 · Nextcloud/WebDAV · SFTP · Azure Blob · and 60+ more

## Architecture Overview

```
┌─────────────────────────────────────────────────────┐
│              ~/Cloud/ (FUSE3 mount)                  │
└────────────────────────┬────────────────────────────┘
                         │  kernel VFS calls
┌────────────────────────▼────────────────────────────┐
│                  stratosync daemon                    │
│  ┌──────────────┐  ┌────────────────┐               │
│  │  FUSE layer  │  │ inotify watcher│               │
│  │   (fuser)    │  │ (notify crate) │               │
│  └──────┬───────┘  └───────┬────────┘               │
│         └─────────┬────────┘                         │
│           ┌───────▼────────┐  ┌──────────────────┐  │
│           │  Sync Engine   │  │  Cache Manager   │  │
│           │  (diff/queue/  │  │  (LRU eviction,  │  │
│           │   conflict)    │  │   quota policy)  │  │
│           └───────┬────────┘  └──────────────────┘  │
│           ┌───────▼────────┐                         │
│           │  SQLite State  │  ← WAL mode             │
│           │  (file index,  │                         │
│           │   sync queue,  │                         │
│           │   xattr store) │                         │
│           └───────┬────────┘                         │
└───────────────────┼─────────────────────────────────┘
                    │  rclone subprocess / pipe
          ┌─────────▼──────────┐
          │  rclone backend    │
          │  (any provider)    │
          └────────────────────┘
```

See [`docs/architecture/`](docs/architecture/) for detailed design documents.

## Quick Start (eventually)

```bash
# 1. Configure a cloud remote
rclone config

# 2. Edit stratosync config to add a mount
$EDITOR ~/.config/stratosync/config.toml

# 3. Test connectivity
stratosync config test

# 4. Start the daemon (or: systemctl --user start stratosyncd)
RUST_LOG=stratosync=debug stratosyncd

# 5. In another terminal — check status
stratosync status
stratosync ls ~/GoogleDrive/Documents
```

## Documentation

- [Architecture Overview](docs/architecture/01-overview.md)
- [FUSE Layer](docs/architecture/02-fuse-layer.md)
- [Sync Engine & State Machine](docs/architecture/03-sync-engine.md)
- [State DB Schema](docs/architecture/04-state-db.md)
- [Backend Abstraction](docs/architecture/05-backend.md)
- [Conflict Resolution](docs/architecture/06-conflict-resolution.md)
- [Cache Management](docs/architecture/07-cache.md)
- [Configuration](docs/architecture/08-config.md)

## Reference Projects Studied

- **[rclone](https://github.com/rclone/rclone)** — backend abstraction and VFS cache design
- **[google-drive-ocamlfuse](https://github.com/astrada/google-drive-ocamlfuse)** — metadata caching patterns for GDrive FUSE
- **[Syncthing BEP](https://docs.syncthing.net/specs/bep-v1.html)** — block-level change detection, conflict model, version vectors
- **[Nextcloud Desktop](https://github.com/nextcloud/desktop)** — conflict resolution UX, sync queue design, journal DB schema

## License

MIT OR Apache-2.0
