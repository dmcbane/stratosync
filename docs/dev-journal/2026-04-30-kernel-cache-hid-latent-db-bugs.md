# Kernel readdir Cache Was Hiding Latent DB Bugs

**Date:** 2026-04-30
**Context:** v0.12.1 added `notify_inval_inode` after every `mkdir`/`create`/`unlink`/`rmdir`/`rename` so Dolphin would refresh its directory listing without the user having to navigate away and back. The change was meant to be cosmetic. It surfaced two real data-integrity bugs that had been hiding in the DB for an unknown length of time.
**Related Commits:** `cf65e2d` (introduced the `inval_inode`), `2f68083` (defensive readdir dedup), `5f5d53e` (root cause + cleanup migration)
**Status:** RESOLVED

## Problem

After v0.12.1 shipped its kernel-cache-invalidation fix for "new folder doesn't appear in Dolphin until you navigate away," the user reported a *new* symptom: visited folders showed up as duplicates at the root of the mount. Two `Desktop`, two `Documents`, two `test`, etc. `ls` confirmed it at the terminal — both were real directory entries pointing at directories with identical content.

But: nothing in the v0.12.1 changeset created file_index rows. We added `inval_inode` to *invalidate* the kernel's cache. How did invalidation cause duplicates to appear?

It didn't. The duplicates had been in the DB all along. The kernel's stale readdir cache had been masking them, and the new `inval_inode` call exposed them by forcing the kernel to ask FUSE for a fresh listing.

## Investigation

Asked the user to dump duplicate `(mount_id, parent_inode, name)` groups via SQLite:

```
1|1|Desktop|2|5:Desktop          | 5681:drives/4139BC84D0721EB5/root:/Desktop
1|1|Documents|2|7:Documents      | 5623:drives/4139BC84D0721EB5/root:/Documents
1|1|test|2|6875:test              | 6877:drives/4139BC84D0721EB5/root:/test
```

The duplicate rows differed only in `remote_path`. One was the clean form (`Desktop`); the other had a `drives/{drive_id}/root:/` prefix. That's a Microsoft Graph parent-reference path format.

The OneDrive delta provider's `resolve_item_path` was stripping `/drive/root:` (default-drive form) but falling through `raw.to_string()` for `/drives/{drive_id}/root:` (specific-drive form, used when the rclone config has a `drive_id` for SharePoint or business OneDrive). The unhandled form leaked the prefix into the persisted `remote_path`. The rclone full-listing poller meanwhile reported the *same* item with its clean relative path. The existing `UNIQUE(mount_id, remote_path)` didn't catch the collision because the path strings differed; both rows survived.

The kernel's readdir cache happened to remember the version it saw first, so the user's directory listing kept showing whichever entry the kernel had cached. Pre-v0.12.1 we never invalidated that cache after a mutation, so they almost never saw both. v0.12.1's `inval_inode` finally forced a fresh fetch and exposed the duplicate row.

## What we shipped

Three commits, in order:

- **`2f68083`** — Defensive `readdir` dedup. Hides any `(parent_inode, name)` duplicates from the kernel by keeping only the lowest-inode row, matching what `get_by_parent_name` (and therefore `lookup`) was already returning. Logs a warning when it fires so we know it happened. This was the band-aid we shipped while we were still investigating.

- **`5f5d53e`** — Root cause: rewrote `resolve_item_path` to handle both `/drive/root:` and `/drives/{drive_id}/root:` via a shared `strip_onedrive_root_prefix` helper. Plus migration `0006_dedupe_file_index.sql` to clean up rows already written by the broken path resolver. The migration:
  1. Reparents children of duplicates onto the keeper (lowest inode in each group).
  2. Deletes the duplicates.
  3. Adds a partial `UNIQUE(mount_id, parent_inode, name) WHERE parent_inode IS NOT NULL` index so future writes can't reintroduce the bug at the schema level.

The defensive `readdir` dedup from `2f68083` stays in place as belt-and-suspenders. With migration 0006 in place it shouldn't fire; if it ever does, the warning surfaces a new path-format edge case for follow-up.

## Bonus bug found in the same pass

While debugging the duplicates we also found a `goutputstream`-rename race that was actively losing user data:

When Dolphin/Nautilus does an overwrite-copy via the standard glib pattern (write to `.goutputstream-XXX`, fsync, then atomic-rename over the destination), our upload finalizer raced the rename:

1. fsync triggers `run_upload`. It captures `entry.cache_path = .goutputstream-XXX` into a local.
2. The rename arrives mid-upload. `tokio::fs::rename` moves the cache file on disk. `db.rename_entry` updates the DB row's `cache_path` to the new name.
3. Upload finishes and calls `db.set_cached(inode, &cache_path /* the captured stale path */, …)`. `set_cached` writes that path back into `cache_path`, *clobbering* the rename's update.
4. The next debounced upload reads the stale path and fails: `cache file missing: …/.goutputstream-XXX`.

Fix: new `StateDb::set_uploaded(inode, cache_size, etag, mtime, size)` that updates everything `set_cached` does *except* `cache_path`. Switched the upload finalizer to it. Existing hydration callers keep using `set_cached` because hydration legitimately writes the cache_path (the file just appeared at it).

## Key Learnings

- **Caches in the kernel are not just performance — they're observability holes.** Stale cached state can mask data-integrity bugs in your own daemon for an arbitrary amount of time. The first time you fix the freshness of one of those caches, audit the data underneath; you may be exposing pre-existing inconsistencies. Add a defensive dedup at the read boundary as a first step before fixing the root cause, so users see consistent behavior immediately while you investigate.
- **Equivalent path forms from cloud APIs are a duplicate-row trap.** Microsoft Graph returns parent-reference paths in two forms (`/drive/root:` and `/drives/{drive_id}/root:`) depending on how the drive is addressed. They mean the same thing semantically but differ as strings, so any `UNIQUE` constraint keyed on the string fails to catch the collision. Audit any backend's "stable identifier" for ambiguity before relying on it as a uniqueness key. When in doubt, *also* maintain a `UNIQUE(parent, name)` invariant, since names are unambiguous within a directory.
- **`set_cached` is the wrong shape for upload finalization.** It was written for hydration, where the cache file *is* being created at a specific path that the DB needs to start tracking. For uploads, the cache file already exists at whatever path the row records — and another caller (rename) might legitimately have moved it during the upload. Reusing `set_cached` for uploads silently corrupted state when those two operations overlapped. New verb (`set_uploaded`) avoids the foot-gun. The general lesson: write paths that mutate identifying state should be separated from paths that mutate non-identifying state.
- **`SQL ORDER BY rowid` and `query_row` are not synonyms.** `lookup` (uses `query_row`) returns one row deterministically; `readdir` (uses `query_map`) returns all rows. When duplicates exist, the two operations diverge in a way that causes "I can `cd` into it but `ls` shows two of them." We exploited this when shipping the dedup band-aid: matching `readdir`'s output to whatever `lookup` would return keeps user-visible state self-consistent.
- **Schema-level invariants beat read-path filters.** Migration 0006 added a partial `UNIQUE(mount_id, parent_inode, name)` index. Once that's in place, no code path can reintroduce `(parent, name)` duplicates without an explicit `INSERT … ON CONFLICT` clause. The `readdir` dedup is now defense-in-depth, not the actual correctness boundary.

## Related Files

- `crates/stratosync-core/src/backend/delta.rs` — `resolve_item_path` and the new `strip_onedrive_root_prefix` helper
- `crates/stratosync-core/src/state/migrations/0006_dedupe_file_index.sql` — one-time cleanup + partial UNIQUE index
- `crates/stratosync-core/src/state/mod.rs` — `set_uploaded`, the goutputstream-race fix
- `crates/stratosync-daemon/src/fuse/mod.rs` — `readdir` dedup band-aid
- `crates/stratosync-daemon/src/sync/upload_queue.rs` — `run_upload` switched from `set_cached` → `set_uploaded`
