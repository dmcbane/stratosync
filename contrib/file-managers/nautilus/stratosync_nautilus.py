"""
Stratosync Nautilus extension — sync-status emblems and context-menu actions.

This file is a thin GObject shell around `stratosync_fm_common`. The same
helper drives the Nemo and Caja extensions; keeping the FM-specific glue
small means a single change to status mapping, menu wording, or CLI
shell-out updates all three desktops.

Installation:
    mkdir -p ~/.local/share/nautilus-python/extensions
    cp -r .../common/stratosync_fm_common.py    ~/.local/share/nautilus-python/extensions/
    cp     .../nautilus/stratosync_nautilus.py  ~/.local/share/nautilus-python/extensions/
    nautilus -q   # restart Nautilus

Requirements:
    - python3-nautilus (nautilus-python), Nautilus 3.0 or 4.0
    - The `stratosync` CLI on PATH for context-menu actions
"""

import os
import sys

# Make the shared helper importable. install.sh / packagers drop both files
# into the same nautilus-python extensions directory; if you're running from
# a source checkout, fall back to the contrib/file-managers/common/ layout.
_HERE = os.path.dirname(os.path.abspath(__file__))
for candidate in (_HERE, os.path.join(_HERE, "..", "common")):
    if os.path.isfile(os.path.join(candidate, "stratosync_fm_common.py")) \
            and candidate not in sys.path:
        sys.path.insert(0, candidate)

import stratosync_fm_common as fm  # noqa: E402

import gi  # noqa: E402

try:
    gi.require_version("Nautilus", "4.0")
    from gi.repository import Nautilus, GObject  # noqa: E402
except ValueError:
    gi.require_version("Nautilus", "3.0")
    from gi.repository import Nautilus, GObject  # noqa: E402


def _path_of(file_info) -> str | None:
    location = file_info.get_location()
    if location is None:
        return None
    return location.get_path()


class StratoSyncInfoProvider(GObject.GObject, Nautilus.InfoProvider):
    """Adds sync-status emblems via the `user.stratosync.status` xattr."""

    def update_file_info(self, file_info):
        path = _path_of(file_info)
        if path is None:
            return
        emblem = fm.emblem_for_status(fm.get_status(path))
        if emblem:
            file_info.add_emblem(emblem)


class StratoSyncMenuProvider(GObject.GObject, Nautilus.MenuProvider):
    """Adds Pin/Unpin and conflict-resolution context-menu items."""

    def get_file_items(self, *args):
        # Nautilus 3.0 signature: (window, files). Nautilus 4.0: (files,).
        files = args[-1] if args else []
        paths = [_path_of(fi) for fi in files]
        items = []
        for action_id, label, tooltip, callback in fm.menu_items_for(paths):
            mi = Nautilus.MenuItem(name=action_id, label=label, tip=tooltip)
            # Nautilus passes (menu_item, *unused) to the activate handler;
            # we accept *_ to be signature-tolerant across 3.0 and 4.0.
            mi.connect("activate", lambda *_args, cb=callback: cb())
            items.append(mi)
        return items

    # Nautilus 3.0 also calls `get_background_items(window, folder)` for
    # right-clicks on empty folder space. We don't currently have any
    # folder-level actions; returning [] is fine.
    def get_background_items(self, *args):
        return []
