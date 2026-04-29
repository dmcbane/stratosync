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
- Use any of rclone's 70+ backends (Google Drive, OneDrive, Dropbox, S3, Nextcloud, SFTP, …)

## Status

**Beta (v0.12.0-beta.1).** Phases 1–6 are functionally complete; encrypted cache is deferred to v0.13.0+. Suitable for daily use on personal cloud storage with the usual beta caveats — back up anything irreplaceable. The daemon provides:

- On-demand FUSE3 filesystem with bidirectional sync
- 3-way text merge for clean conflict resolution; `.conflict.*` siblings under `.stratosync-conflicts/` for everything else
- Delta-API polling (Google Drive `pageToken`, OneDrive `/delta`)
- Pin/unpin for offline; range-read fast path; readdir prefetch
- Live dashboard TUI (`stratosync dashboard`)
- Selective sync via per-mount `ignore_patterns`
- Bandwidth scheduling via per-mount `upload_window`
- File versioning with CLI list/restore
- Prometheus `/metrics` endpoint (opt-in)
- Nautilus emblem extension and `stratosync-tray` system-tray indicator
- Distribution packages (`.deb`, `.rpm`, AUR) for x86_64 and ARM64

See the [CHANGELOG](CHANGELOG.md) for release-by-release detail.

## Documentation

- **[Installation guide](docs/installation.md)** — prerequisites, build/install, systemd, first mount.
- **[Usage guide](docs/usage.md)** — daily workflow, every CLI command, selective sync, bandwidth schedules, versioning, dashboard, conflicts.
- **[Configuration reference](docs/architecture/08-config.md)** — every TOML setting, with examples and limitations.
- **[Architecture](docs/architecture/01-overview.md)** — design docs (FUSE layer, sync engine, state DB, backend, conflicts, cache).
- **[Roadmap](docs/ROADMAP.md)** — what shipped and what's next.

## Quick start

```bash
# 1. Configure your cloud provider via rclone (one-time)
rclone config
rclone lsd <remote>:    # verify connectivity

# 2. Install
git clone https://github.com/dmcbane/stratosync
cd stratosync
./install.sh

# 3. Edit ~/.config/stratosync/config.toml — at minimum, set name/remote/mount_path

# 4. Start the daemon
systemctl --user start stratosyncd
ls ~/GoogleDrive/
```

For the full walkthrough, see **[docs/installation.md](docs/installation.md)**.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for build instructions, testing guide, and code layout.

## License

Licensed under either of

- [Apache License, Version 2.0](LICENSE-APACHE)
- [MIT License](LICENSE-MIT)

at your option.
