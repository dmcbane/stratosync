# OneDrive delta: skip items with unrecognized `parentReference.path`

**Date:** 2026-05-07
**Version:** v0.13.0-beta.4

## Symptom

`OneDriveDelta::resolve_item_path` (in
`crates/stratosync-core/src/backend/delta.rs`) had a fallback branch
that, when neither `/drive/root:` nor `/drives/{drive_id}/root:` matched
the raw `parentReference.path`, logged a warn and **passed the raw
string through as the parent path**. Concatenated with the item name,
this produced rows like:

- `parent_path = ""`, name=`x` → stored as `x` at the mount root
- `parent_path = "/drives/.../items/01ABC"`, name=`y` → stored as
  `drives/.../items/01ABC/y` (a remote_path that doesn't exist on the
  backend)

No live failure was reported in this session — the diff sat in the
working tree as a half-finished diagnostic enhancement (id/name/deleted
fields added to the warn) inherited from the v0.13.0-beta.2 milestone
work. The user asked what it was for, and the comment itself flagged
the underlying risk: "the current pass-through risks placing
genuinely-nested items at our mount root."

## Why this is the same class as the SharePoint duplicate-folder bug

The SharePoint fix (v0.12.x, see
`test_onedrive_resolve_drives_id_root_prefix`) added handling for the
`/drives/{drive_id}/root:` shape because items addressed that way had
been falling through the same warn-and-pass-through fallback. Result:
two rows per item — one from the delta channel with a malformed
`drives/{id}/root:/test` `remote_path`, one from the rclone poller
with the clean `test`. UNIQUE didn't catch them because `remote_path`
differed.

That fix recognized one specific shape. The fallback for
*unrecognized* shapes was left structurally identical: warn, then
trust raw and hope. Any future Graph response we haven't seen
(item-ID references, recyclebin paths, shared-with-me, special
folders) hits the same trap.

## Fix

Replace the pass-through with `return None` in the unrecognized branch,
so unknown shapes are skipped entirely. Keep the enriched warn (id,
name, deleted, is_folder, is_file) so the soak signal remains
diagnosable.

Why skipping is safe:

- **Live items**: rclone's full-listing poller runs at the same
  `poll_interval` and reports the same items with their clean
  relative paths. A skipped delta event delays processing by at most
  one poll cycle. No data loss.
- **Deleted items**: the v0.13 milestone-3 dispatcher
  (`crates/stratosync-daemon/src/sync/poller.rs:527`) already tries
  `delete_remote_entry_by_item_id` first, but only if the event
  reaches it. With this fix, deletes for unknown-shape items are
  dropped on the floor at the parser level. The fallback safety net
  is the verifying-stat self-heal in `do_hydrate`
  (v0.12.3 / `2026-05-06-onedrive-delta-rename-stale-paths.md`),
  which prunes stale rows on the next read attempt. Slower, but
  bounded.
- **No silent corruption**: today's behavior can store a malformed
  `remote_path`, attach a real inode to it, and feed it to readdir.
  Skipping prevents that.

## What we explicitly chose NOT to do

- **Add per-shape handlers for recyclebin/shared-with-me.** No live
  evidence yet that these shapes show up; designing for them
  speculatively risks cargo-cult code. The warn is the soak signal.
  When a real shape rises above noise, add the matcher to
  `strip_onedrive_root_prefix` and a parallel test.
- **Translate item-ID references via an extra Graph round-trip.**
  Same reason — no evidence the `/drives/{id}/items/{id}` form
  actually appears in `parentReference.path` for delta-channel items.
  An extra HTTP per unrecognized event would also be a poor cost
  trade.
- **Promote the warn to an error / return SyncError.** The whole
  delta page would fail, regressing the nameless-item lesson from
  v0.12.x (one weird item shouldn't kill 999 good ones).

## Tests

`crates/stratosync-core/src/backend/delta.rs`:

- `test_onedrive_resolve_unknown_prefix_skipped` — `/special/recyclebin/items/abc` → None
- `test_onedrive_resolve_empty_parent_path_skipped` — `parent_reference.path = None` → None
- `test_onedrive_resolve_drives_items_id_form_skipped` — `/drives/{id}/items/{id}` → None
- `test_onedrive_resolve_unknown_prefix_does_not_crash_with_special_chars` — quotes, newlines in raw path

The four tests fail against the pre-fix pass-through (item lands at
mount root or with a malformed parent), pass against the skip
behavior.

## Soak target

The new warn (`OneDrive parentReference.path has unrecognized shape;
skipping item …`) should fire essentially never on healthy mounts. If
we see it in the field with non-trivial frequency, that's a real
shape Graph is emitting that deserves explicit support. Ack the warn
to telemetry / dashboard if the rate becomes load-bearing.
