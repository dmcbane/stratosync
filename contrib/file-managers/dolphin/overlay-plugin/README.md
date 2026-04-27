# Stratosync Dolphin / Konqueror emblem-overlay plugin (KF6)

Adds sync-status emblem overlays in Dolphin and Konqueror, matching the
Nautilus / Nemo / Caja extensions exactly. Same emblem set, same
freedesktop icon names, same `user.stratosync.status` xattr underneath.

This is a separate piece from the
[`stratosync.desktop`](../stratosync.desktop) ServiceMenu — that one is a
declarative right-click menu and ships in the main package; this is a
real C++/Qt plugin that needs a CMake build, so it lives in its own
subpackage (or skip step in the installer).

## What you get

| Sync status | Emblem icon |
|-------------|-------------|
| `cached`    | `emblem-default` (green check) |
| `dirty`     | `emblem-synchronizing` (sync arrows) |
| `uploading` | `emblem-synchronizing` |
| `hydrating` | `emblem-downloads` |
| `remote`    | `emblem-web` (cloud) |
| `conflict`  | `emblem-important` (yellow warning) |
| `stale`     | `emblem-generic` |

Non-stratosync files get nothing — the plugin no-ops on any path that
doesn't carry `user.stratosync.status`.

## Build prerequisites

- CMake ≥ 3.16
- A C++17 compiler (g++ ≥ 7, clang ≥ 5)
- Qt6 (Core + Gui) development headers, ≥ 6.5
- KF6 KIO development headers, ≥ 6.0
- `extra-cmake-modules` (ECM)

Distro packages:

```bash
# Fedora / RHEL / Rocky / Alma (EPEL on RHEL family)
sudo dnf install cmake gcc-c++ extra-cmake-modules \
    qt6-qtbase-devel kf6-kio-devel

# Debian / Ubuntu (KDE Plasma 6)
sudo apt install cmake g++ extra-cmake-modules \
    qt6-base-dev libkf6kio-dev

# Arch Linux
sudo pacman -S cmake gcc extra-cmake-modules \
    qt6-base kio
```

Plasma 5 / KF5 are **not supported by this plugin**. KF5 is upstream-EOL
and the API surface for `KOverlayIconPlugin` is different enough that a
ported version belongs in a separate file. If you need Plasma 5
emblems, the [`stratosync.desktop`](../stratosync.desktop) ServiceMenu
gives you context-menu actions without overlays.

## Build and install

```bash
cd contrib/file-managers/dolphin/overlay-plugin
cmake -B build -S .
cmake --build build
sudo cmake --install build
```

The plugin lands at:

- Fedora/Rocky/Alma: `/usr/lib64/qt6/plugins/kf6/overlayicon/stratosyncoverlay.so`
- Debian/Ubuntu: `/usr/lib/x86_64-linux-gnu/qt6/plugins/kf6/overlayicon/stratosyncoverlay.so`
- Arch: `/usr/lib/qt6/plugins/kf6/overlayicon/stratosyncoverlay.so`

Then either restart Dolphin or rebuild the KSyCoCa cache:

```bash
kbuildsycoca6
# Restart Dolphin so it picks up the new plugin:
kquitapp6 dolphin && dolphin &
```

## How it works

KIO's `KCoreDirLister` calls `KOverlayIconPlugin::getOverlays(QUrl)` for
every file it lists. We:

1. Reject anything that isn't a local file (can't `getxattr` a network URL).
2. Read `user.stratosync.status` via `getxattr(2)`.
3. Map the status string to a freedesktop emblem icon name.

That's the whole flow — synchronous, ~1 µs per call (the FUSE layer
serves xattrs from its in-memory state DB, no network round-trip). No
daemon talk, no IPC.

## Per-user install (without root)

```bash
cd contrib/file-managers/dolphin/overlay-plugin
cmake -B build -S . \
    -DCMAKE_INSTALL_PREFIX=$HOME/.local \
    -DKDE_INSTALL_PLUGINDIR=$HOME/.local/lib/qt6/plugins
cmake --build build
cmake --install build
```

You may also need to add the directory to KIO's plugin search path:

```bash
export QT_PLUGIN_PATH="$HOME/.local/lib/qt6/plugins:${QT_PLUGIN_PATH:-}"
# Add to ~/.config/plasma-workspace/env/ for it to persist across sessions.
```

## Troubleshooting

- **No emblems appear.** Run `kbuildsycoca6` and restart Dolphin. Verify
  the `.so` is in the right plugin directory: `find / -name 'stratosyncoverlay.so' 2>/dev/null`.
- **`getOverlays not found` symbol error.** You linked against KF5
  by mistake — check `ldd /path/to/stratosyncoverlay.so | grep KF` and
  rebuild against KF6.
- **Emblems wrong vs. Nautilus.** If a status maps to a different icon
  here vs. the GTK extensions, fix it in `stratosync_overlay.cpp` →
  `emblemFor()` to match `stratosync_fm_common.py` → `EMBLEM_MAP`.
