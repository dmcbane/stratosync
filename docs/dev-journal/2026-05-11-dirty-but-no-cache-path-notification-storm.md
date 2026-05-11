# 2026-05-11 — "dirty but no cache_path" notification storm

## Symptom
Desktop notifications firing in bursts after every daemon restart:

```
stratosync: upload failed
Failed to upload <filename>: backend fatal: inode <N>: dirty but no cache_path
```

User report observed 277 rows in `gdrive.db` stuck in this state,
mostly Joplin Notes (`.md` files in `Documents/JoplinNotes`) and a
handful of files under `Documents/DungeonDraft_packs`, `Equipment`,
`ImportantDocuments`, `LifeEssentials`, `eBooks`.

Triage query:

```sql
SELECT inode, kind, status, cache_path IS NULL, parent_inode, name
FROM file_index
WHERE status='dirty' AND cache_path IS NULL AND kind='file';
```

## Root cause
`setattr(size=N)` (FUSE truncate, also the path `open(O_TRUNC)` takes
through the kernel) on a never-hydrated file. The handler at
`fuse/mod.rs:setattr` synthesized a cache_path, created the file at
that path, and called `db.set_dirty_size(ino, new_size)` — which
updates `status`, `size`, `cache_size` but **not** `cache_path`. The
row was left as `status='dirty', cache_path=NULL` while a real cache
file existed on disk.

The upload queue then:
1. `get_pending_uploads` returns the row.
2. `run_upload` loads it, hits `cache_path = NULL`, returns
   `Fatal("inode N: dirty but no cache_path")`.
3. The fatal handler at `upload_queue.rs` sets the row to `dirty`
   (already dirty — no-op) and fires `notification::send`.
4. On daemon restart, `reconcile_orphans` removes the on-disk cache
   file (no DB row points at it), making recovery impossible from
   local content. The row stays poisoned and the cycle repeats on
   every restart.

The original "dirty but no cache_path" guard had a directory-only
sibling (`reset_stuck_dirty_directories`) but no equivalent for the
file-level case. The setattr code path predated that sweep.

## Fix
Three-part — `crates/stratosync-core/src/state/mod.rs` and the two
consumer call sites.

1. **New `set_dirty_with_cache_path(inode, cache_path, size)`** —
   atomic update of all four fields. `setattr`'s "create cache
   file" branch now calls this instead of `set_dirty_size`.
2. **New `reset_stuck_dirty_files_without_cache_path()`** —
   symmetric startup sweep called from `main.rs` alongside the
   existing directory cleanup. Reverts poisoned rows to
   `status='remote'` so the next open() re-hydrates from the cloud.
   The local "dirty" state was never uploaded; cloud is the source
   of truth.
3. **Defensive self-heal in `run_upload`** — if a row with
   `cache_path = NULL` ever reaches the queue again, log + reset
   to `remote` + return Ok. Prevents the fatal/notification cycle
   from re-emerging if some new code path regresses into the same
   shape.

## Follow-up
- The setattr fix only covers the `size` branch; other setattr
  fields (mode, uid, gid, times) are still accepted-and-discarded.
  Not in scope here.
- Joplin's "edit a never-opened note" workflow is the canonical
  reproducer. Worth keeping in mind for any future "writes through
  a synthesized cache_path" surfaces (e.g., a hypothetical
  `write_at_offset_to_remote_only` path).
