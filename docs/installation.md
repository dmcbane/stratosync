# Installation

This guide takes you from a clean Linux system to a mounted cloud drive. If you already have rclone configured, skip to step 2.

## Prerequisites

| Requirement | Why | How to install |
|-------------|-----|----------------|
| **Linux with FUSE3** | The daemon mounts via FUSE3. | `sudo apt install fuse3` (Debian/Ubuntu) or `sudo dnf install fuse3` (Fedora) |
| **rclone** | Backend for every cloud provider. | [rclone.org/install](https://rclone.org/install/) |
| **Rust 1.80+** | Only required if building from source. | `rustup update stable` |
| **libfuse3-dev** | Only required if building from source. | `sudo apt install libfuse3-dev` (Debian/Ubuntu) or `sudo dnf install fuse3-devel` (Fedora) |

Stratosync ships pre-built packages for **x86_64** and **ARM64** Linux. If you're on one of those, prefer the package install; the source path is for unsupported platforms or development.

---

## 1. Configure rclone

Stratosync uses [rclone](https://rclone.org/) for every backend operation. You need a working rclone remote before stratosync can mount it.

```bash
rclone config
```

Follow the interactive prompts. Provider-specific guides:

- [Google Drive](https://rclone.org/drive/)
- [OneDrive](https://rclone.org/onedrive/)
- [Dropbox](https://rclone.org/dropbox/)
- [Amazon S3](https://rclone.org/s3/)
- [Nextcloud / WebDAV](https://rclone.org/webdav/)
- [Full provider list](https://rclone.org/overview/)

Verify the remote works before continuing:

```bash
rclone lsd <remote_name>:    # list top-level directories
rclone about <remote_name>:  # show storage quota
```

> **Multiple accounts of the same provider** (e.g. personal + work Google Drive)
> are configured as two distinct rclone remotes — `gdrive-personal` and
> `gdrive-work` — and then two `[[mount]]` blocks in stratosync. Per-mount
> isolation is total: separate database, cache directory, OAuth token, and
> upload queue. No daemon-side flag is needed.

---

## 2. Install stratosync

### Option A: Pre-built packages (recommended)

Replace `0.12.0` with the [latest release version](https://github.com/dmcbane/stratosync/releases/latest) and `amd64`/`x86_64` with `arm64`/`aarch64` for ARM:

```bash
# Debian / Ubuntu
curl -LO https://github.com/dmcbane/stratosync/releases/latest/download/stratosync_0.12.0_amd64.deb
sudo dpkg -i stratosync_0.12.0_amd64.deb

# Fedora / RHEL
curl -LO https://github.com/dmcbane/stratosync/releases/latest/download/stratosync-0.12.0-1.x86_64.rpm
sudo dnf install ./stratosync-0.12.0-1.x86_64.rpm

# Arch Linux (AUR)
yay -S stratosync-git

# Any distro (tarball)
curl -LO https://github.com/dmcbane/stratosync/releases/latest/download/stratosync-0.12.0-linux-x86_64.tar.gz
tar xzf stratosync-0.12.0-linux-x86_64.tar.gz
sudo cp stratosync-0.12.0-linux-x86_64/stratosync* /usr/local/bin/
```

### Option B: Build from source

```bash
git clone https://github.com/dmcbane/stratosync
cd stratosync
./install.sh
```

`install.sh` builds the workspace in release mode, copies `stratosyncd` (daemon) and `stratosync` (CLI) to `~/.local/bin/`, drops a default config at `~/.config/stratosync/config.toml`, and enables a `stratosyncd` systemd user service.

For a manual build:

```bash
cargo build --release --workspace
install -m 755 target/release/stratosyncd      ~/.local/bin/
install -m 755 target/release/stratosync       ~/.local/bin/
install -m 755 target/release/stratosync-tray  ~/.local/bin/    # optional: system tray
```

### Optional: file-manager integrations

`install.sh` and the prebuilt packages auto-detect which of these have
their Python bindings installed and ship the matching extension; nothing
to do beyond installing the binding for your desktop.

- **Nautilus** (GNOME) — `python3-nautilus` / `nautilus-python` /
  `python-nautilus`. Sync-status emblems and context-menu actions
  (Pin / Unpin / Resolve conflict).
- **Nemo** (Cinnamon) — `python3-nemo` / `nemo-python`. Same emblems
  and context-menu actions as Nautilus.
- **Caja** (MATE) — `python3-caja` / `python-caja`. Same emblems and
  context-menu actions.
- **System tray** — run `stratosync-tray` (built alongside the CLI;
  autostart `.desktop` shipped in packages).
- **Dolphin (KDE), Thunar (XFCE), PCManFM** — pending Phase 6 follow-ups
  in [ROADMAP.md](ROADMAP.md).

For manual install (without the installer or a package), copy the helper
plus the per-FM file into the extensions directory for each desktop:

```bash
mkdir -p ~/.local/share/nautilus-python/extensions
cp contrib/file-managers/common/stratosync_fm_common.py    ~/.local/share/nautilus-python/extensions/
cp contrib/file-managers/nautilus/stratosync_nautilus.py   ~/.local/share/nautilus-python/extensions/
nautilus -q
```

Repeat with `nemo-python` / `caja-python` paths for the other two.

---

## 3. Configure a mount

Edit `~/.config/stratosync/config.toml`:

```toml
[daemon]
log_level = "info"

[[mount]]
name          = "gdrive"             # unique per mount; used for the per-mount DB
remote        = "gdrive:/"           # rclone remote and path (from step 1)
mount_path    = "~/GoogleDrive"      # where files appear in your filesystem
cache_quota   = "10 GiB"             # max local cache size
poll_interval = "30s"                # how often to check for remote changes
enabled       = true
```

Add as many `[[mount]]` blocks as you want — one per cloud account. Optional per-mount fields you'll likely care about:

- `ignore_patterns = ["*.tmp", "node_modules/**"]` — selective sync (see [usage guide](usage.md#selective-sync)).
- `upload_window = "22:00-06:00"` — only upload at night.
- `version_retention = 10` — keep N file versions for rollback.

The full set is documented in **[architecture/08-config.md](architecture/08-config.md)**.

Test the configuration before starting the daemon:

```bash
stratosync config test
```

This verifies that every enabled mount's rclone remote is reachable.

---

## 4. Start the daemon

### Option A: systemd (recommended for daily use)

```bash
systemctl --user enable --now stratosyncd
systemctl --user status stratosyncd      # check it's running
journalctl --user -u stratosyncd -f      # follow logs
```

> **Don't enable `PrivateTmp=true` or `NoNewPrivileges=true` in the unit file.**
> `PrivateTmp` creates a private mount namespace that hides the FUSE mount from
> every other process; `NoNewPrivileges` blocks the setuid `fusermount3` binary
> from performing the mount. The shipped service unit intentionally omits both.

### Option B: foreground (for development / debugging)

```bash
RUST_LOG=stratosync=debug stratosyncd
```

Either way, your cloud files are now available at the configured mount path:

```bash
ls ~/GoogleDrive/
```

For day-to-day commands (`status`, `dashboard`, `conflicts`, `pin`, `versions`, …), see the **[usage guide](usage.md)**.

---

## Uninstalling

```bash
systemctl --user disable --now stratosyncd
fusermount3 -u ~/GoogleDrive          # unmount each configured mount
rm -rf ~/.config/stratosync ~/.cache/stratosync ~/.local/share/stratosync

# If installed via package:
sudo apt remove stratosync   # or dnf, pacman, etc.

# If installed via install.sh:
rm -f ~/.local/bin/stratosync ~/.local/bin/stratosyncd ~/.local/bin/stratosync-tray
rm -f ~/.config/systemd/user/stratosyncd.service
systemctl --user daemon-reload
```

Removing `~/.cache/stratosync` deletes the local content cache; remote data is
untouched. Removing `~/.local/share/stratosync` deletes the per-mount SQLite
databases (file index, version history, change tokens) — the daemon will re-
populate from the remote on next start.
