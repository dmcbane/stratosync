"""
Stratosync Nemo extension — sync-status emblems and context-menu actions.

Cinnamon's Nemo file manager exposes a Python extension API
(`gi.repository.Nemo`) that mirrors the Nautilus 3.0 API. This file is
the same shell as `stratosync_nautilus.py` with the `gi` flavour swapped;
all real logic lives in the shared `stratosync_fm_common` module.

Installation:
    mkdir -p ~/.local/share/nemo-python/extensions
    cp .../common/stratosync_fm_common.py ~/.local/share/nemo-python/extensions/
    cp .../nemo/stratosync_nemo.py        ~/.local/share/nemo-python/extensions/
    nemo -q   # restart Nemo

Requirements:
    - python3-nemo (`nemo-python`)
    - The `stratosync` CLI on PATH for context-menu actions
"""

import os
import sys

_HERE = os.path.dirname(os.path.abspath(__file__))
for candidate in (_HERE, os.path.join(_HERE, "..", "common")):
    if os.path.isfile(os.path.join(candidate, "stratosync_fm_common.py")) \
            and candidate not in sys.path:
        sys.path.insert(0, candidate)

import stratosync_fm_common as fm  # noqa: E402

import gi  # noqa: E402
gi.require_version("Nemo", "3.0")
from gi.repository import Nemo, GObject  # noqa: E402


def _path_of(file_info) -> str | None:
    location = file_info.get_location()
    if location is None:
        return None
    return location.get_path()


class StratoSyncNemoInfoProvider(GObject.GObject, Nemo.InfoProvider):
    """Adds sync-status emblems via the `user.stratosync.status` xattr."""

    def update_file_info(self, file_info):
        path = _path_of(file_info)
        if path is None:
            return
        emblem = fm.emblem_for_status(fm.get_status(path))
        if emblem:
            file_info.add_emblem(emblem)


class StratoSyncNemoMenuProvider(GObject.GObject, Nemo.MenuProvider):
    """Pin/Unpin and conflict-resolution context-menu items."""

    def get_file_items(self, *args):
        # Nemo passes (window, files); be tolerant in case that ever changes.
        files = args[-1] if args else []
        paths = [_path_of(fi) for fi in files]
        items = []
        for action_id, label, tooltip, callback in fm.menu_items_for(paths):
            mi = Nemo.MenuItem(name=action_id, label=label, tip=tooltip)
            mi.connect("activate", lambda *_args, cb=callback: cb())
            items.append(mi)
        return items

    def get_background_items(self, *args):
        return []
