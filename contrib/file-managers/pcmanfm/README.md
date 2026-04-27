# Stratosync PCManFM / PCManFM-Qt actions (LXDE / LXQt)

Adds Pin / Unpin / Resolve-conflict items to the right-click menu in
PCManFM (LXDE) and PCManFM-Qt (LXQt) via the FreeDesktop file-manager
Actions specification.

> **Heads up — actions only, no emblems.** PCManFM has no emblem overlay
> API. This snippet ships only the context-menu integration.

## Installation

PCManFM and PCManFM-Qt both honour the FreeDesktop Actions spec, which
defines a per-user drop-in directory at
`~/.local/share/file-manager/actions/` (and a system-wide
`/usr/share/file-manager/actions/`). Each `.desktop` file is one
context-menu action.

```bash
# Per-user
mkdir -p ~/.local/share/file-manager/actions
cp stratosync-*.desktop ~/.local/share/file-manager/actions/

# System-wide (packagers do this)
sudo install -Dm644 -t /usr/share/file-manager/actions/ stratosync-*.desktop
```

Restart PCManFM (`pcmanfm -q && pcmanfm &`) or just open a new window.

> **Some PCManFM-Qt builds also look at** `~/.config/pcmanfm-qt/actions/` —
> if the menu doesn't appear after the standard install, copy the files
> there as well.

## Available actions

| Action | What it runs | When it appears |
|--------|--------------|-----------------|
| Pin for offline use | `stratosync pin <path>` | Always |
| Unpin (allow eviction) | `stratosync unpin <path>` | Always |
| Resolve conflict — keep my version | `stratosync conflicts keep-local <path>` | Always |
| Resolve conflict — keep remote version | `stratosync conflicts keep-remote <path>` | Always |
| Resolve conflict — 3-way merge (text) | `stratosync conflicts merge <path>` | Text-like MIME only |

The Actions spec can filter by MIME type but not by xattr, so we leave
filtering to the wrapper script: `stratosync-fm-action` exits cleanly
with a desktop notification if the path isn't under a stratosync mount,
making the items harmless on non-managed files.

## How it works

Each `.desktop` file is a tiny declarative action. PCManFM merges them
into the right-click menu and runs the `Exec=` field when clicked —
that's `stratosync-fm-action`, the shared shell wrapper that:

1. Checks the path carries `user.stratosync.status`.
2. Dispatches to the right `stratosync` CLI subcommand.
3. Detaches via `setsid` so PCManFM doesn't block.

Both `stratosync` and `stratosync-fm-action` need to be on PATH. The
project installer puts them in `~/.local/bin/`.

## Troubleshooting

- **Items don't appear.** Verify the spec dir is right for your
  PCManFM-Qt version (some look at `~/.config/pcmanfm-qt/actions/`).
  Restart PCManFM. Check the `.desktop` files have valid syntax with
  `desktop-file-validate` if installed.
- **Action does nothing.** Run from a terminal:
  `stratosync-fm-action pin ~/GoogleDrive/foo.pdf`. The wrapper writes
  any error to stderr.
