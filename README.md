# stratosync

**A first-class Linux cloud sync daemon** — on-demand virtual filesystem, multi-backend, conflict-aware.

Stratosync brings the OneDrive/Google Drive desktop experience to Linux: files appear immediately in your filesystem, data is fetched on-demand when you open a file, changes upload automatically in the background, and conflicts are preserved rather than silently destroyed.

```
~/GoogleDrive/                  ← FUSE3 virtual mount
├── Documents/
│   ├── report.pdf              ← hydrated (local cache)
│   └── draft.docx              ← placeholder (metadata only, fetched on open)
└── Photos/
    └── 2025/                   ← directory listing from remote index
```

## Why stratosync?

Linux has excellent cloud sync tools, but none that provide all three: **on-demand hydration**, **native filesystem integration**, and **conflict safety**.

| Approach | On-demand | Native FS | Conflict-safe | Multi-backend |
|----------|:---------:|:---------:|:-------------:|:-------------:|
| **stratosync** | yes | yes (FUSE) | yes | yes (70+ via rclone) |
| rclone mount | yes | yes (FUSE) | no | yes |
| rclone bisync | no (full copy) | no (CLI) | partial | yes |
| Insync / OverGrive | no | partial | partial | Google/OneDrive only |
| GNOME Online Accounts | no | GVFS only | no | limited |

**Use stratosync if you want to:**

- Access cloud files as a normal directory (`ls`, `cat`, `vim`, `cp` — any tool works)
- Avoid downloading your entire cloud storage locally
- Edit files and have changes sync automatically
- Keep working if two machines edit the same file (conflicts create `.conflict` siblings instead of overwriting)
- Use any of rclone's 70+ backends (Google Drive, OneDrive, Dropbox, S3, Nextcloud, SFTP, ...)

## Status

🚧 **Pre-alpha (v0.6.0)** — Phase 1 (read-only VFS) and Phase 2 (bidirectional sync) are implemented and tested with Google Drive. The daemon mounts, lists, reads, writes, renames, and deletes files. Conflict resolution, LRU cache eviction, and inotify-based change detection are functional. See the [CHANGELOG](CHANGELOG.md) for details.

## Prerequisites

- **Linux** with FUSE3 support
- **Rust 1.80+** — `rustup update stable`
- **rclone** — [https://rclone.org/install/](https://rclone.org/install/)
- **libfuse3-dev** — `sudo apt install libfuse3-dev` (Debian/Ubuntu) or `sudo dnf install fuse3-devel` (Fedora)

## Setup

### 1. Configure rclone

Stratosync uses [rclone](https://rclone.org/) as its backend for cloud provider access. You need a working rclone remote before stratosync can mount it.

```bash
rclone config
```

Follow the interactive prompts to add your cloud provider. See the [rclone documentation](https://rclone.org/docs/) for provider-specific setup:

- [Google Drive](https://rclone.org/drive/)
- [OneDrive](https://rclone.org/onedrive/)
- [Dropbox](https://rclone.org/dropbox/)
- [S3](https://rclone.org/s3/)
- [Nextcloud/WebDAV](https://rclone.org/webdav/)
- [Full provider list](https://rclone.org/overview/)

Verify your remote works:

```bash
rclone lsd <remote_name>:    # list top-level directories
rclone about <remote_name>:  # show storage quota
```

### 2. Install stratosync

```bash
git clone https://github.com/dmcbane/stratosync
cd stratosync
./install.sh
```

This builds the project, installs `stratosyncd` (daemon) and `stratosync` (CLI) to `~/.local/bin/`, creates a default config at `~/.config/stratosync/config.toml`, and enables a systemd user service.

### 3. Configure a mount

Edit `~/.config/stratosync/config.toml`:

```toml
[daemon]
log_level = "info"

[[mount]]
name          = "gdrive"        # unique name for this mount
remote        = "gdrive:/"      # rclone remote and path (from step 1)
mount_path    = "~/GoogleDrive" # where files appear in your filesystem
cache_quota   = "10 GiB"       # max local cache size
poll_interval = "30s"           # how often to check for remote changes
```

You can add multiple `[[mount]]` sections for different cloud accounts.

Test the configuration:

```bash
stratosync config test
```

### 4. Start the daemon

**Option A — systemd (recommended for daily use):**

```bash
systemctl --user start stratosyncd
systemctl --user status stratosyncd    # check it's running
journalctl --user -u stratosyncd -f    # view logs
```

> **Note on systemd sandboxing:** The service unit intentionally omits
> `PrivateTmp=true` and `NoNewPrivileges=true`. `PrivateTmp` creates a private
> mount namespace that makes the FUSE mount invisible outside the daemon, and
> `NoNewPrivileges` blocks the setuid `fusermount3` binary from performing the
> mount. If you customize the unit file, do not re-enable these directives.

**Option B — foreground (for development/debugging):**

```bash
RUST_LOG=stratosync=debug stratosyncd
```

Your cloud files are now accessible at the mount path (e.g., `~/GoogleDrive/`).

## Usage

Once mounted, the cloud directory works like any local directory:

```bash
# Browse files
ls ~/GoogleDrive/Documents/

# Read files (downloaded on-demand)
cat ~/GoogleDrive/Documents/report.pdf

# Edit files (changes upload automatically)
vim ~/GoogleDrive/Documents/notes.md

# Create, copy, move, delete — all work
cp local-file.txt ~/GoogleDrive/
mv ~/GoogleDrive/old.txt ~/GoogleDrive/new.txt
rm ~/GoogleDrive/temp.txt
mkdir ~/GoogleDrive/NewFolder
```

### CLI commands

```bash
stratosync status           # sync status across all mounts
stratosync ls [path]        # list remote contents with sync status
stratosync config show      # print current configuration
stratosync config test      # verify connectivity to all remotes
stratosync config edit      # open config in $EDITOR
stratosync conflicts        # list files with sync conflicts
```

### How it works

- **On-demand hydration**: files appear in the mount immediately with correct names and sizes. The actual content is downloaded from the cloud only when you open the file. Subsequent reads use the local cache.
- **Automatic upload**: when you write to a file, changes are saved to the local cache immediately and uploaded to the cloud in the background after a short debounce window (default 2 seconds). `fsync()` triggers an immediate upload.
- **Conflict resolution**: if you and someone else edit the same file, the remote version wins the canonical path and your local version is saved as `filename.conflict.20260403T120000Z.a1b2c3d4.ext`.
- **Cache management**: the local cache is bounded by `cache_quota`. When the cache fills up, least-recently-used files are evicted (their data is deleted locally but remains in the cloud). Pinned files are never evicted.
- **Change detection**: an inotify watcher detects local changes; a polling loop detects remote changes. Future versions will support provider delta APIs for faster detection.

### Configuration reference

```toml
[daemon]
log_level = "info"           # trace, debug, info, warn, error

[daemon.fuse]
threads         = 4          # FUSE worker threads
attr_timeout_s  = 5          # seconds to cache file attributes
entry_timeout_s = 5          # seconds to cache directory entries
allow_other     = false      # allow other users to access the mount

[daemon.sync]
max_hydration_concurrent = 4    # parallel downloads
max_upload_concurrent    = 2    # parallel uploads
upload_debounce_ms       = 2000 # wait before uploading after last write
upload_close_debounce_ms = 500  # wait after file close
text_conflict_strategy   = "keep_both"  # or "merge"

[[mount]]
name          = "gdrive"
remote        = "gdrive:/"
mount_path    = "~/GoogleDrive"
cache_quota   = "10 GiB"
poll_interval = "30s"
enabled       = true

[mount.rclone]
extra_flags = []             # extra flags for rclone commands
bwlimit     = "10M"         # bandwidth limit (optional)
transfers   = 4              # parallel rclone transfers (optional)

[mount.eviction]
policy    = "lru"            # lru, lfu, size_desc, never
low_mark  = 0.80             # evict down to this fraction of quota
high_mark = 0.90             # start evicting at this fraction
```

## Architecture

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

## Documentation

- [Architecture Overview](docs/architecture/01-overview.md)
- [FUSE Layer](docs/architecture/02-fuse-layer.md)
- [Sync Engine & State Machine](docs/architecture/03-sync-engine.md)
- [State DB Schema](docs/architecture/04-state-db.md)
- [Backend Abstraction](docs/architecture/05-backend.md)
- [Conflict Resolution](docs/architecture/06-conflict-resolution.md)
- [Cache Management](docs/architecture/07-cache.md)
- [Configuration](docs/architecture/08-config.md)
- [Changelog](CHANGELOG.md)

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for build instructions, testing guide, and code layout.

## License

Licensed under either of

- [Apache License, Version 2.0](LICENSE-APACHE)
- [MIT License](LICENSE-MIT)

at your option.
