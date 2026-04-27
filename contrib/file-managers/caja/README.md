# Stratosync Caja extension

Sync-status emblem overlays and context-menu actions for MATE's Caja file
manager. Functionally identical to the Nautilus and Nemo extensions; same
emblems, same menu items, same shell-out to the `stratosync` CLI.

## Prerequisites

```bash
# Debian / Ubuntu / Mint MATE
sudo apt install python3-caja

# Fedora
sudo dnf install python3-caja

# Arch
sudo pacman -S python-caja
```

## Installation

```bash
mkdir -p ~/.local/share/caja-python/extensions
cp ../common/stratosync_fm_common.py  ~/.local/share/caja-python/extensions/
cp stratosync_caja.py                 ~/.local/share/caja-python/extensions/
caja -q   # restart Caja
```

System-wide install path is `/usr/share/caja-python/extensions/`.

## How it works

Identical to the Nautilus/Nemo extensions — this file is a thin shell
over `stratosync_fm_common`, swapping only the `gi.repository.Nautilus`
import for `gi.repository.Caja`. Caja's Python API mirrors Nautilus 3.0
closely, so the wrappers are nearly line-for-line.

See [`../nautilus/README.md`](../nautilus/README.md) for the emblem table
and a description of the available context-menu actions.

## Troubleshooting

- **No emblems showing.** Run `caja -q && caja --no-desktop` and watch
  stderr for Python import errors. Confirm xattrs work via
  `getfattr -n user.stratosync.status ~/GoogleDrive/somefile`.
- **Actions do nothing.** Confirm `stratosync` is on `PATH` in your
  desktop session.
