"""
Shared logic for stratosync file-manager extensions (Nautilus / Nemo / Caja).

Each extension is a thin GObject shell that imports its own `gi` flavour
(`gi.repository.Nautilus` vs `Nemo` vs `Caja`) and forwards into the helpers
here. The actual work — reading xattrs, mapping status to emblems, deciding
which context-menu items to show, shelling out to the `stratosync` CLI for
actions — lives in this module so we don't keep three slightly-different
copies of the same code.

This module has **no `gi` imports** and no Nautilus/Nemo/Caja dependency,
so it's importable from a plain Python and unit-testable in isolation.
"""

import logging
import os
import shlex
import shutil
import subprocess
from typing import Iterable, List, Optional

log = logging.getLogger("stratosync.fm")

# ── xattr names exposed by the FUSE layer (read-only) ──────────────────────────
XATTR_STATUS      = b"user.stratosync.status"
XATTR_ETAG        = b"user.stratosync.etag"
XATTR_REMOTE_PATH = b"user.stratosync.remote_path"

# ── Status → emblem icon name (freedesktop standard names) ─────────────────────
EMBLEM_MAP = {
    "cached":    "emblem-default",        # green checkmark — synced
    "dirty":     "emblem-synchronizing",  # sync arrows — pending upload
    "uploading": "emblem-synchronizing",  # sync arrows — uploading
    "hydrating": "emblem-downloads",      # download arrow — downloading
    "remote":    "emblem-web",            # cloud/globe — not cached
    "conflict":  "emblem-important",      # yellow warning — conflict
    "stale":     "emblem-generic",        # neutral — needs re-sync
}


# ── xattr helpers ──────────────────────────────────────────────────────────────

def _read_xattr(path: str, name: bytes) -> Optional[str]:
    try:
        return os.getxattr(path, name).decode("utf-8")
    except (OSError, AttributeError):
        pass
    # Fall back to the third-party `xattr` package (older Pythons / non-Linux).
    try:
        import xattr  # type: ignore
        return xattr.getxattr(path, name.decode()).decode("utf-8")
    except Exception:
        return None


def get_status(path: str) -> Optional[str]:
    """Return the current sync status string, or None if the file is not under
    a stratosync mount (no xattr present)."""
    return _read_xattr(path, XATTR_STATUS)


def get_etag(path: str) -> Optional[str]:
    return _read_xattr(path, XATTR_ETAG)


def get_remote_path(path: str) -> Optional[str]:
    return _read_xattr(path, XATTR_REMOTE_PATH)


def is_stratosync_path(path: str) -> bool:
    """Cheap check used to decide whether to render emblems / menu items at
    all. A stratosync-managed file always exposes `user.stratosync.status`."""
    return get_status(path) is not None


def emblem_for_status(status: Optional[str]) -> Optional[str]:
    if status is None:
        return None
    return EMBLEM_MAP.get(status)


# ── CLI shell-out ──────────────────────────────────────────────────────────────
#
# We deliberately shell out to the `stratosync` CLI binary rather than
# extending the daemon IPC protocol. Subprocess startup is fine at human-
# click cadence (~10s of ms) and this keeps the extensions independent of
# IPC schema versioning. If we ever care about latency we can add IPC ops
# later; the helper API in this module won't change.

def _stratosync_bin() -> Optional[str]:
    return shutil.which("stratosync")


def _run_cli(args: List[str], wait: bool = False) -> bool:
    """Invoke `stratosync <args>` in the background. Returns True if the
    process was launched successfully (NOT whether the command itself
    succeeded — file managers can't sensibly block on that)."""
    bin_ = _stratosync_bin()
    if bin_ is None:
        log.warning("`stratosync` binary not found on PATH; cannot run %s",
                    " ".join(args))
        return False
    cmd = [bin_, *args]
    log.info("running: %s", " ".join(shlex.quote(a) for a in cmd))
    try:
        if wait:
            subprocess.run(cmd, check=False, timeout=30)
        else:
            # Detach: don't wait, don't inherit our stdio, don't leave a
            # zombie when the file manager exits.
            subprocess.Popen(
                cmd,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                start_new_session=True,
            )
        return True
    except (OSError, subprocess.SubprocessError) as exc:
        log.warning("failed to spawn stratosync %s: %s", args, exc)
        return False


def pin(path: str) -> bool:
    return _run_cli(["pin", path])


def unpin(path: str) -> bool:
    return _run_cli(["unpin", path])


def resolve_keep_local(path: str) -> bool:
    return _run_cli(["conflicts", "keep-local", path])


def resolve_keep_remote(path: str) -> bool:
    return _run_cli(["conflicts", "keep-remote", path])


def resolve_merge(path: str) -> bool:
    return _run_cli(["conflicts", "merge", path])


# ── Menu-item descriptions ─────────────────────────────────────────────────────
#
# Returning a list of (id, label, tip, callback) tuples keeps each file-
# manager wrapper as a tiny adapter that just translates these into native
# MenuItem objects. The wrappers must NOT decide which items to show — that
# logic lives here so all three backends behave identically.

def menu_items_for(paths: Iterable[str]):
    """Yield (action_id, label, tooltip, callback) tuples for the
    context-menu items that should be shown when the user has selected
    `paths`. Filters internally — paths outside stratosync are dropped, and
    items only appear when they make sense for the current statuses.

    Callbacks take no arguments and act on the *closed-over* selection."""
    paths = [p for p in paths if p and is_stratosync_path(p)]
    if not paths:
        return

    statuses = {get_status(p) for p in paths}

    # Pin / unpin always available — the daemon is idempotent on already-
    # pinned and never-pinned paths, and there is no `pinned` xattr today
    # to gate the choice on.
    yield (
        "stratosync::pin",
        "Pin for offline use",
        "Download and keep these items available offline",
        lambda: [pin(p) for p in paths],
    )
    yield (
        "stratosync::unpin",
        "Unpin (remove offline copy)",
        "Allow these items to be evicted from the local cache",
        lambda: [unpin(p) for p in paths],
    )

    # Conflict resolution — only when at least one selected file is in
    # `conflict` status. Acting on a non-conflict file via these would
    # confuse users.
    if "conflict" in statuses:
        conflicting = [p for p in paths if get_status(p) == "conflict"]
        yield (
            "stratosync::resolve_keep_local",
            "Resolve conflict — keep my version",
            "Upload the local version, drop the remote conflict file",
            lambda: [resolve_keep_local(p) for p in conflicting],
        )
        yield (
            "stratosync::resolve_keep_remote",
            "Resolve conflict — keep remote version",
            "Discard local changes; download the remote version",
            lambda: [resolve_keep_remote(p) for p in conflicting],
        )
        yield (
            "stratosync::resolve_merge",
            "Resolve conflict — 3-way merge (text)",
            "Attempt automatic merge via git merge-file (text files only)",
            lambda: [resolve_merge(p) for p in conflicting],
        )
