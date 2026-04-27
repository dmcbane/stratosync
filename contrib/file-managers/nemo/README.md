# Stratosync Nemo extension

Sync-status emblem overlays and context-menu actions for Cinnamon's Nemo
file manager. Functionally identical to the Nautilus extension; same
emblems, same menu items, same shell-out to the `stratosync` CLI.

## Prerequisites

```bash
# Debian / Ubuntu (Linux Mint)
sudo apt install python3-nemo

# Fedora
sudo dnf install nemo-python

# Arch
sudo pacman -S nemo-python
```

## Installation

```bash
mkdir -p ~/.local/share/nemo-python/extensions
cp ../common/stratosync_fm_common.py  ~/.local/share/nemo-python/extensions/
cp stratosync_nemo.py                 ~/.local/share/nemo-python/extensions/
nemo -q   # restart Nemo
```

System-wide install path is `/usr/share/nemo-python/extensions/`.

## How it works

Identical to the Nautilus extension — this file is a thin shell over
`stratosync_fm_common`, swapping only the `gi.repository.Nautilus` import
for `gi.repository.Nemo`. Nemo's Python API mirrors Nautilus 3.0 closely,
so the wrappers are nearly line-for-line.

See [`../nautilus/README.md`](../nautilus/README.md) for the emblem table
and a description of the available context-menu actions.

## Troubleshooting

- **No emblems showing.** `nemo -q && nemo --no-desktop` and watch stderr
  for Python import errors. Confirm xattrs work via
  `getfattr -n user.stratosync.status ~/GoogleDrive/somefile`.
- **Actions do nothing.** Confirm `stratosync` is on `PATH` in your
  desktop session.
