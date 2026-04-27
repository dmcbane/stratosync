# Stratosync Nautilus extension

Adds sync-status emblem overlays and context-menu actions (pin/unpin, conflict
resolution) to GNOME's Nautilus file manager.

## Status icons

| Status | Emblem | Meaning |
|--------|--------|---------|
| cached | checkmark | File is synced and cached locally |
| dirty | sync arrows | Local changes pending upload |
| uploading | sync arrows | Upload in progress |
| hydrating | download arrow | Download in progress |
| remote | cloud/globe | Metadata only, not cached |
| conflict | yellow warning | Concurrent-edit conflict |
| stale | neutral | Remote changed; needs re-sync |

## Context-menu actions

Right-click any file or directory under a stratosync mount:

- **Pin for offline use** / **Unpin** — wraps `stratosync pin`/`unpin`.
- **Resolve conflict — keep my version** / **keep remote version** /
  **3-way merge (text)** — appear only when at least one selected file
  is in `conflict` state. Wrap the corresponding `stratosync conflicts …`
  subcommand.

Actions shell out to the `stratosync` CLI in the background. Make sure it's
on `PATH` for the user running Nautilus.

## Prerequisites

```bash
# Debian / Ubuntu
sudo apt install python3-nautilus

# Fedora
sudo dnf install nautilus-python

# Arch
sudo pacman -S python-nautilus
```

## Installation

Both this file *and* the shared helper need to land in the same Nautilus
extensions directory:

```bash
mkdir -p ~/.local/share/nautilus-python/extensions
cp ../common/stratosync_fm_common.py  ~/.local/share/nautilus-python/extensions/
cp stratosync_nautilus.py             ~/.local/share/nautilus-python/extensions/
nautilus -q   # restart Nautilus
```

Or use the project installer, which does this for you:

```bash
./install.sh
```

## How it works

- **Emblems** come from the `user.stratosync.status` xattr that the FUSE
  layer exposes on every file in a stratosync mount. No D-Bus, no IPC —
  read straight from the kernel.
- **Context-menu actions** shell out to the `stratosync` CLI. We could
  extend the daemon IPC protocol with action ops, but subprocess launch is
  cheap at human-click cadence and keeps the extension independent of
  IPC schema versioning.

## Troubleshooting

- **No emblems showing.** Verify the extension loaded:
  `nautilus -q && NAUTILUS_DEBUG=1 nautilus`. Confirm xattrs are visible
  with `getfattr -n user.stratosync.status ~/GoogleDrive/somefile`.
- **Menu items missing.** They only render under stratosync mounts (where
  the xattr is present). Conflict-resolution items only appear when at
  least one selected file has `status=conflict`.
- **Action does nothing.** Check that `stratosync` is on `PATH` in your
  desktop session. Run a manual `stratosync pin <path>` to see any
  underlying error.
- **Wrong Nautilus version.** The extension tries 4.0 first, falls back
  to 3.0.
