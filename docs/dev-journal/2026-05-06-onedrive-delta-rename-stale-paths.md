# OneDrive delta misses renames; stale rows loop reads EAGAIN forever

**Date:** 2026-05-06
**Version:** v0.12.3

## Symptom

User copying a large remote file to local (via Dolphin/Nautilus or
plain `cp`) saw the operation stall for tens of minutes, eventually
failing — or simply hanging until they killed it. The daemon logged:

```
read: not found: Attempt 3/3 failed with 1 errors and: directory not found
range download failed, waiting for full hydration: not found:  ino=2179
read: transient error (will retry): hydration failed: 5 fh=374
```

Always the same inode. The journal was the only place this surfaced
(v0.12.2 added tray/dashboard visibility — see
[2026-05-06-hydration-health-visibility.md](2026-05-06-hydration-health-visibility.md)
— but didn't fix the underlying loop).

## Root cause

OneDrive's Microsoft Graph delta endpoint emits **`Modified` events with
the new path** when a file (or its parent directory) is renamed —
**without** a paired `Deleted` event for the old path. Our delta
processor in `crates/stratosync-core/src/backend/delta.rs` faithfully
turns those into `RemoteChange::Added` / `Modified`, which then go
through `StateDb::upsert_remote_file_gen` (`crates/stratosync-core/src/state/mod.rs:248`).

That upsert matches on `(mount_id, remote_path)`. A new path means a
new row with a new inode — and **the old row stays in the DB
untouched**. There's no generation sweep in delta mode (`generation = 0`
is hard-coded at `crates/stratosync-daemon/src/sync/poller.rs:504`), so
nothing prunes the stale rows.

When the user opened a file via the FUSE mount, lookup-by-name returned
the **old** stale row (its inode happened to be lower, so `get_by_parent_name`
preferred it via the inode-asc tie-break in v0.12.1's `idx_file_index_parent_name_unique`).
Reads on that handle called `do_hydrate` with the old `remote_path`,
rclone dutifully tried `/old-parent/file.iso`, the remote returned
"directory not found" (exit code 3, → `SyncError::NotFound`), the read
returned EAGAIN, and the user's `cp` retried — forever.

## Why this is delta-specific

The rclone full-poll path (`poll_once`) bumps `poll_generation` on every
remote entry it sees, then `delete_stale_entries(mount_id, generation)`
sweeps anything not seen this poll. A renamed file under rclone gets
upserted at its new path *and* the old-path row is purged because its
generation didn't get bumped. So full-poll mounts (Google Drive without
delta, Dropbox, S3) self-correct within one poll cycle. OneDrive, which
is the only delta-enabled provider today, does not.

## Fix (v0.12.3)

In `do_hydrate` (`crates/stratosync-daemon/src/fuse/mod.rs`), when
`backend.download(...)` returns `SyncError::NotFound`, fire a one-shot
`backend.stat(&entry.remote_path)`:

- **stat also returns NotFound**: confirmed stale. `db.delete_entry(inode)`
  + return `NotFound` (which maps to `ENOENT` in `errno()`, not `EAGAIN`).
  The user's `cp` gets a clean error, the kernel-level lookup re-fetches,
  and the next directory listing repopulates the row at its new path
  with a fresh inode.
- **stat returns anything else** (Ok or Network/Transient): the row is
  probably fine. Treat the original failure as transient — let normal
  retry sort it out. Avoids data loss from pruning over a momentary
  rclone glitch.

Two non-obvious bits:

### Why we don't act on Network/Transient errors

Pruning the row on a network blip would lose the user's metadata mid-
edit. The fix only triggers on NotFound — an unambiguous signal from the
backend "I looked, the path isn't there." For everything else, retry
remains the right answer.

### Why we skip the `set_status(... Remote)` rollback when pruning

After `delete_entry`, the row no longer exists. `set_status` on a
deleted inode silently affects zero rows — harmless but confusing in
logs. We track the prune with a local `stale_path_pruned` flag and
skip the rollback when it fires.

## What this does NOT fix

The **proper** fix is in the delta processor: track items by their
stable Microsoft Graph item ID, and when an item's path changes, update
the existing row's `remote_path` (via `rename_entry` or a new
`update_remote_path` method) instead of inserting a new row. That
requires a schema change (we don't currently store the item ID) and is
a bigger refactor — left for v0.13.x.

The v0.12.3 fix handles every observed symptom (cp loop, dashboard
warning, journal noise) by making stale rows self-evict on first
attempted use. It's a band-aid, but a load-bearing one until the
delta-by-id rewrite lands.

## Tests

`crates/stratosync-daemon/src/fuse/mod.rs::hydrate_tests`:
- `do_hydrate_stale_path_is_pruned_after_notfound` — file in DB, never
  on remote: hydrate fails NotFound, row gone afterward.
- `do_hydrate_keeps_entry_when_remote_still_has_path` — file in DB
  AND seeded remotely, but `fail_download_not_found` makes download
  fail anyway: hydrate fails, row survives.

The second test required a new `MockBackend::fail_download_not_found(path)`
helper (`crates/stratosync-core/src/backend/mod.rs`) so we could
simulate "stat OK, download NotFound" without affecting other tests.
