# Stratosync KDE service menu (Dolphin / Konqueror)

Adds a **Stratosync** submenu to the right-click menu in Dolphin and
Konqueror with Pin / Unpin / Resolve-conflict actions.

> **Two pieces.** This `.desktop` file is the *context-menu* integration
> (right-click → Stratosync). Sync-status **emblem overlays** are a
> separate KF6 C++ plugin under
> [`overlay-plugin/`](overlay-plugin/README.md) — real CMake build,
> separate subpackage in `.deb`/`.rpm`. Either is useful on its own; both
> together give Dolphin parity with the GTK file managers.

## Installation

```bash
# Per-user
mkdir -p ~/.local/share/kio/servicemenus
cp stratosync.desktop ~/.local/share/kio/servicemenus/
chmod +x ~/.local/share/kio/servicemenus/stratosync.desktop

# System-wide (packagers do this)
sudo install -Dm755 stratosync.desktop /usr/share/kio/servicemenus/stratosync.desktop
```

> Some KDE versions also load `~/.local/share/kservices5/ServiceMenus/`
> — if Dolphin doesn't see the menu after a restart, try copying it
> there as well.

The actions are scoped under `MimeType=all/all;` so they appear on every
file and folder; the wrapper script `stratosync-fm-action` no-ops with a
desktop notification when invoked on a path that isn't under a stratosync
mount, so this is harmless clutter rather than confusing failure.

Restart Dolphin (close and reopen the window) for the menu to appear.

## Available actions

All five group under a single **Stratosync** submenu so they don't
clutter the top-level right-click menu:

| Action | What it runs |
|--------|--------------|
| Pin for offline use | `stratosync pin <path>` |
| Unpin (allow eviction) | `stratosync unpin <path>` |
| Resolve conflict — keep my version | `stratosync conflicts keep-local <path>` |
| Resolve conflict — keep remote version | `stratosync conflicts keep-remote <path>` |
| Resolve conflict — 3-way merge (text) | `stratosync conflicts merge <path>` |

All five are visible regardless of the selected file's status. Acting on
a non-conflict file via the resolve-conflict items just exits cleanly
with a notification — the daemon is forgiving.

## How it works

KDE service menus are pure `.desktop` files; no compilation or Python
runtime needed. KIO scans `~/.local/share/kio/servicemenus/` (and the
system-wide `/usr/share/kio/servicemenus/`) and renders any
`Type=Service`, `ServiceTypes=KonqPopupMenu/Plugin` entry as a
context-menu item.

Each action's `Exec=` runs `stratosync-fm-action`, the shared shell
wrapper that:

1. Checks the path actually carries `user.stratosync.status` (i.e. it
   *is* under a stratosync mount), surfacing a notification if not.
2. Dispatches to the right `stratosync` CLI subcommand.
3. Detaches via `setsid` so Dolphin never blocks waiting for the action
   to finish.

Make sure both `stratosync` and `stratosync-fm-action` are on `PATH`
for the user running Dolphin (the project installer puts both in
`~/.local/bin/`).

## Troubleshooting

- **Menu doesn't appear.** Restart Dolphin. If it still doesn't show,
  run `kbuildsycoca5` (or `kbuildsycoca6` on Plasma 6) to rebuild the
  KSyCoCa cache.
- **Action does nothing.** Ensure `stratosync-fm-action` and
  `stratosync` are on the desktop session's PATH. Run the action from
  a terminal manually to see any underlying error: `stratosync-fm-action
  pin ~/GoogleDrive/foo.pdf`.
