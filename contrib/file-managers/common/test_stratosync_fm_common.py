"""Pure-Python unit tests for the file-manager helper. No `gi` needed.

Run with:  python3 -m pytest contrib/file-managers/common/
or         python3 contrib/file-managers/common/test_stratosync_fm_common.py
"""

import os
import sys
import tempfile
import unittest
from unittest import mock

# Allow direct script execution via `python3 test_stratosync_fm_common.py`.
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import stratosync_fm_common as fm  # noqa: E402


class XattrHelpersTest(unittest.TestCase):

    def test_status_missing_xattr_returns_none(self):
        with tempfile.NamedTemporaryFile() as tf:
            self.assertIsNone(fm.get_status(tf.name))
            self.assertFalse(fm.is_stratosync_path(tf.name))

    def test_status_round_trip(self):
        # Skip on filesystems / kernels that reject user.* xattrs.
        with tempfile.NamedTemporaryFile() as tf:
            try:
                os.setxattr(tf.name, fm.XATTR_STATUS, b"cached")
            except (OSError, AttributeError):
                self.skipTest("filesystem does not support user.* xattrs")
            self.assertEqual(fm.get_status(tf.name), "cached")
            self.assertTrue(fm.is_stratosync_path(tf.name))

    def test_emblem_mapping_complete(self):
        # Every status the FUSE layer can emit should have an emblem.
        for status in ("cached", "dirty", "uploading", "hydrating",
                       "remote", "conflict", "stale"):
            self.assertIsNotNone(
                fm.emblem_for_status(status),
                f"missing emblem for status={status}",
            )

    def test_emblem_unknown_status_returns_none(self):
        self.assertIsNone(fm.emblem_for_status(None))
        self.assertIsNone(fm.emblem_for_status("not-a-real-status"))


class MenuLogicTest(unittest.TestCase):
    """The menu-item generator drives all three FM extensions identically.
    Filtering / gating logic lives here, not in the wrappers."""

    def test_no_items_for_non_stratosync_path(self):
        with mock.patch.object(fm, "is_stratosync_path", return_value=False):
            self.assertEqual(list(fm.menu_items_for(["/tmp/foo"])), [])

    def test_pin_unpin_always_offered_for_stratosync_path(self):
        with mock.patch.object(fm, "is_stratosync_path", return_value=True), \
             mock.patch.object(fm, "get_status", return_value="cached"):
            ids = [item[0] for item in fm.menu_items_for(["/mnt/foo"])]
        self.assertIn("stratosync::pin", ids)
        self.assertIn("stratosync::unpin", ids)
        # Conflict items must NOT appear when there's no conflict.
        self.assertNotIn("stratosync::resolve_keep_local", ids)
        self.assertNotIn("stratosync::resolve_merge", ids)

    def test_conflict_items_appear_only_when_a_file_is_in_conflict(self):
        with mock.patch.object(fm, "is_stratosync_path", return_value=True), \
             mock.patch.object(fm, "get_status", return_value="conflict"):
            ids = [item[0] for item in fm.menu_items_for(["/mnt/c"])]
        self.assertIn("stratosync::resolve_keep_local", ids)
        self.assertIn("stratosync::resolve_keep_remote", ids)
        self.assertIn("stratosync::resolve_merge", ids)

    def test_mixed_selection_offers_conflict_items_for_conflicting_only(self):
        # With one conflicting + one cached file selected, conflict items
        # appear but their callbacks should target only the conflicting
        # path.
        statuses = {"/mnt/a": "cached", "/mnt/b": "conflict"}
        with mock.patch.object(fm, "is_stratosync_path", return_value=True), \
             mock.patch.object(fm, "get_status", side_effect=statuses.get), \
             mock.patch.object(fm, "resolve_keep_local") as keep_local:
            items = {item[0]: item for item in fm.menu_items_for(list(statuses))}
            self.assertIn("stratosync::resolve_keep_local", items)
            items["stratosync::resolve_keep_local"][3]()
            keep_local.assert_called_once_with("/mnt/b")


class CliShelloutTest(unittest.TestCase):

    def test_run_cli_returns_false_when_binary_missing(self):
        with mock.patch.object(fm, "_stratosync_bin", return_value=None):
            self.assertFalse(fm._run_cli(["pin", "/mnt/x"]))

    def test_pin_invokes_stratosync_pin(self):
        with mock.patch.object(fm, "_stratosync_bin",
                               return_value="/fake/stratosync"), \
             mock.patch.object(fm.subprocess, "Popen") as popen:
            self.assertTrue(fm.pin("/mnt/x"))
        cmd = popen.call_args[0][0]
        self.assertEqual(cmd, ["/fake/stratosync", "pin", "/mnt/x"])


if __name__ == "__main__":
    unittest.main()
