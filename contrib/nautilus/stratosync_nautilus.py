"""
Stratosync Nautilus Extension — sync status emblem overlays.

Reads the `user.stratosync.status` extended attribute from FUSE-mounted
files and displays corresponding emblem icons in Nautilus.

Installation:
    mkdir -p ~/.local/share/nautilus-python/extensions
    cp stratosync_nautilus.py ~/.local/share/nautilus-python/extensions/
    nautilus -q  # restart Nautilus

Requirements:
    - python3-nautilus (nautilus-python)
    - python3-xattr (or python3-pyxattr)
"""

import os

try:
    import gi
    gi.require_version('Nautilus', '4.0')
    from gi.repository import Nautilus, GObject
    HAS_NAUTILUS_4 = True
except ValueError:
    try:
        gi.require_version('Nautilus', '3.0')
        from gi.repository import Nautilus, GObject
        HAS_NAUTILUS_4 = False
    except Exception:
        raise ImportError("python3-nautilus is required")

# Map stratosync status values to standard emblem icon names.
# These are the freedesktop/GNOME icon names available on most distros.
EMBLEM_MAP = {
    'cached':    'emblem-default',        # green checkmark — synced
    'dirty':     'emblem-synchronizing',  # sync arrows — pending upload
    'uploading': 'emblem-synchronizing',  # sync arrows — uploading
    'hydrating': 'emblem-downloads',      # download arrow — downloading
    'remote':    'emblem-web',            # cloud/globe — not cached
    'conflict':  'emblem-important',      # yellow warning — conflict
    'stale':     'emblem-generic',        # neutral — needs re-sync
}


def _get_status(path):
    """Read user.stratosync.status xattr from a file path."""
    try:
        # Try os.getxattr (Python 3.3+, Linux only)
        return os.getxattr(path, b'user.stratosync.status').decode('utf-8')
    except (OSError, AttributeError):
        pass

    # Fallback: try the xattr package
    try:
        import xattr
        return xattr.getxattr(path, 'user.stratosync.status').decode('utf-8')
    except Exception:
        return None


class StratoSyncInfoProvider(GObject.GObject, Nautilus.InfoProvider):
    """Nautilus extension that adds sync status emblems to files."""

    def update_file_info(self, file_info):
        location = file_info.get_location()
        if location is None:
            return

        path = location.get_path()
        if path is None:
            return

        status = _get_status(path)
        if status is None:
            return

        emblem = EMBLEM_MAP.get(status)
        if emblem:
            file_info.add_emblem(emblem)
