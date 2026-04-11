# Stratosync Nautilus Extension

Adds sync status emblem overlays to files in GNOME's Nautilus file manager.

## Status Icons

| Status | Emblem | Meaning |
|--------|--------|---------|
| cached | checkmark | File is synced and cached locally |
| dirty | sync arrows | Local changes pending upload |
| uploading | sync arrows | Upload in progress |
| hydrating | download arrow | Download in progress |
| remote | cloud/globe | Metadata only, not cached |
| conflict | yellow warning | Concurrent edit conflict |
| stale | neutral | Remote changed, needs re-sync |

## Prerequisites

```bash
# Ubuntu/Debian
sudo apt install python3-nautilus

# Fedora
sudo dnf install nautilus-python

# Arch
sudo pacman -S python-nautilus
```

## Installation

```bash
mkdir -p ~/.local/share/nautilus-python/extensions
cp stratosync_nautilus.py ~/.local/share/nautilus-python/extensions/
nautilus -q   # restart Nautilus
```

Or use the stratosync installer:

```bash
./install.sh   # includes Nautilus extension installation
```

## How It Works

The extension reads the `user.stratosync.status` extended attribute that
stratosync's FUSE layer exposes on every mounted file. This attribute
contains the sync status as a string (e.g., "cached", "dirty", "conflict").

The extension maps each status to a standard freedesktop emblem icon name.
No network access or D-Bus communication is needed — everything is read
from the filesystem's xattr interface.

## Troubleshooting

**No emblems showing?**
- Verify the extension is loaded: `nautilus -q && NAUTILUS_DEBUG=1 nautilus`
- Check xattrs work: `getfattr -n user.stratosync.status ~/GoogleDrive/somefile`
- Ensure `python3-nautilus` is installed

**Wrong Nautilus version?**
The extension tries Nautilus 4.0 first, then falls back to 3.0.
