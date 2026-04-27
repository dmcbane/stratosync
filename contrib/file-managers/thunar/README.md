# Stratosync Thunar custom actions (XFCE)

Adds Pin / Unpin / Resolve-conflict items to Thunar's right-click menu
via Thunar's built-in Custom Actions (`uca`).

> **Heads up — actions only, no emblems.** Thunar has no native emblem
> overlay API. Emblems would require either a thumbnailer-based status
> badge or a sidebar panel; both are tracked as Phase 6 follow-ups.
> This snippet ships only the context-menu integration.

## Installation

Thunar reads a *single* XML file at `~/.config/Thunar/uca.xml`. There is
no drop-in directory, so we ship a snippet you merge into your existing
file — auto-merging risks clobbering custom actions you already have.

### If you already have a `uca.xml`

Merge the `<action>` blocks from `stratosync-uca.xml` into your existing
file's `<actions>` root. A quick way using xmlstarlet (or just an
editor):

```bash
# Inspect what's currently in your uca.xml
cat ~/.config/Thunar/uca.xml

# Open both files side-by-side and copy each <action>…</action> block
# from stratosync-uca.xml into the <actions>…</actions> root of yours.
$EDITOR ~/.config/Thunar/uca.xml
```

`<unique-id>` values for stratosync's actions all start with `stratosync-`
— if you ever want to remove them, search for that prefix.

### If you don't have a `uca.xml` yet

```bash
mkdir -p ~/.config/Thunar
cat > ~/.config/Thunar/uca.xml <<'EOF'
<?xml encoding="UTF-8" version="1.0"?>
<actions>
EOF
cat stratosync-uca.xml >> ~/.config/Thunar/uca.xml
echo "</actions>" >> ~/.config/Thunar/uca.xml
```

Then restart Thunar (`thunar -q && thunar &`).

### Or use the GUI

Thunar's *Edit → Configure custom actions…* dialog can import / edit
actions one at a time if you'd rather not touch the XML.

## Available actions

| Action | What it runs |
|--------|--------------|
| Stratosync: Pin for offline use | `stratosync pin <path>` |
| Stratosync: Unpin | `stratosync unpin <path>` |
| Stratosync: Resolve conflict — keep my version | `stratosync conflicts keep-local <path>` |
| Stratosync: Resolve conflict — keep remote version | `stratosync conflicts keep-remote <path>` |
| Stratosync: Resolve conflict — 3-way merge | `stratosync conflicts merge <path>` |

All five render on every right-click — Thunar UCA has no xattr-based
filtering. Acting on a non-stratosync file just exits cleanly with a
notification (the wrapper script `stratosync-fm-action` handles this
gracefully).

## How it works

Each `<action>` in `uca.xml` runs the shared `stratosync-fm-action`
shell wrapper, which:

1. Checks the path carries `user.stratosync.status` (i.e. it's under a
   stratosync mount), surfacing a notification if not.
2. Dispatches to the right `stratosync` CLI subcommand.
3. Detaches via `setsid` so Thunar doesn't block.

Both `stratosync` and `stratosync-fm-action` need to be on the desktop
session's PATH. The project installer puts them in `~/.local/bin/`.

## Troubleshooting

- **Items don't appear.** Check `~/.config/Thunar/uca.xml` is valid XML
  (`xmllint --noout ~/.config/Thunar/uca.xml`). Restart Thunar
  (`thunar -q && thunar &`).
- **Action does nothing.** Run it from a terminal:
  `stratosync-fm-action pin ~/GoogleDrive/foo.pdf`. The wrapper writes
  any error to stderr.
