"""
Stratosync Caja extension — sync-status emblems and context-menu actions.

MATE's Caja file manager exposes a Python extension API
(`gi.repository.Caja`) that mirrors the Nautilus 3.0 API. This file is
the same shell as `stratosync_nautilus.py` and `stratosync_nemo.py` with
the `gi` flavour swapped; all real logic lives in the shared
`stratosync_fm_common` module.

Installation:
    mkdir -p ~/.local/share/caja-python/extensions
    cp .../common/stratosync_fm_common.py ~/.local/share/caja-python/extensions/
    cp .../caja/stratosync_caja.py        ~/.local/share/caja-python/extensions/
    caja -q   # restart Caja

Requirements:
    - python3-caja (`caja-python`)
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
gi.require_version("Caja", "2.0")
from gi.repository import Caja, GObject  # noqa: E402


def _path_of(file_info) -> str | None:
    location = file_info.get_location()
    if location is None:
        return None
    return location.get_path()


class StratoSyncCajaInfoProvider(GObject.GObject, Caja.InfoProvider):
    """Adds sync-status emblems via the `user.stratosync.status` xattr."""

    def update_file_info(self, file_info):
        path = _path_of(file_info)
        if path is None:
            return
        emblem = fm.emblem_for_status(fm.get_status(path))
        if emblem:
            file_info.add_emblem(emblem)


class StratoSyncCajaMenuProvider(GObject.GObject, Caja.MenuProvider):
    """Pin/Unpin and conflict-resolution context-menu items."""

    def get_file_items(self, *args):
        files = args[-1] if args else []
        paths = [_path_of(fi) for fi in files]
        items = []
        for action_id, label, tooltip, callback in fm.menu_items_for(paths):
            mi = Caja.MenuItem(name=action_id, label=label, tip=tooltip)
            mi.connect("activate", lambda *_args, cb=callback: cb())
            items.append(mi)
        return items

    def get_background_items(self, *args):
        return []
