# 2026-05-11 — file row cache_path invariant (migration 0010)

## Context
Follow-up to the same-day setattr fix. The beta.12 commit closed the
known producer of `(kind='file', status='dirty', cache_path=NULL)`
poisoned rows. A focused audit of every "set status to
cached/dirty/uploading" call site asked: are there *other* paths
that could reach the same shape? Two findings:

1. `cli/commands/versions.rs:restore` had exactly the setattr bug
   shape — synthesized a cache_path, wrote the blob there, then
   called `set_status(Dirty)` without persisting the path.
2. The other production sites (`fuse/write_ops.rs::handle_create`,
   `upload_queue.rs::run_upload`'s `set_status(Uploading)`, the
   upsert CASE preservation in `upsert_remote_file_*`,
   `reset_uploading`, conflict creation) were clean — every one
   either provided a cache_path or set a status (`remote`,
   `hydrating`, `stale`, `conflict`) that the invariant excludes.

## What I did
Three reinforcing changes — none replaces the runtime self-heal
landed in beta.12; they upgrade the system from "recover" to
"prevent + detect."

### 1. DB invariant (migration 0010)
SQLite doesn't support `ALTER TABLE ADD CHECK CONSTRAINT`, so two
BEFORE triggers on `file_index` carry the invariant:

```sql
CREATE TRIGGER file_index_dirty_requires_cache_path_insert
BEFORE INSERT ON file_index
WHEN NEW.kind = 'file'
  AND NEW.status IN ('cached','dirty','uploading')
  AND NEW.cache_path IS NULL
BEGIN
  SELECT RAISE(ABORT,
    'file_index invariant: status in (cached,dirty,uploading) requires cache_path');
END;
-- same shape for UPDATE
```

The migration is paired with a cleanup pass that reverts existing
poisoned rows to `remote` *before* the triggers go live — without
it, a pre-fix DB would refuse to migrate (no, the triggers don't
validate existing rows on creation, but any subsequent UPDATE
into the same shape would fire). Belt-and-suspenders with the
runtime `reset_stuck_dirty_files_without_cache_path` sweep.

Statuses NOT covered:
- `remote` / `hydrating` — no local content by definition.
- `stale` — `upsert_remote_file_*` legitimately flips a
  never-hydrated `remote` row to `stale` on a later poll visit;
  cache_path stays NULL.
- `conflict` — conflict-file rows can be remote-only (the
  `.conflict.{ts}.{hash}` sibling isn't pinned locally;
  `sync/conflict.rs:255` sets `cache_path: None` deliberately).

### 2. Read-side filter on `get_pending_uploads`
Added `AND cache_path IS NOT NULL` to the SELECT. Even if a row
somehow ends up poisoned (legacy data, direct `sqlite3` edits,
some future bug that bypasses the trigger), the upload queue
physically cannot see it — no fatal error, no notification, no
self-heal needed.

### 3. `versions::restore` switched to `set_dirty_with_cache_path`
The bug shape mirrors setattr; the fix is the same. With the
trigger now in place, a regression here would fail loudly in the
test suite instead of producing a 277-row backlog in production.

## Audit table (production sites that mark file rows
cached/dirty/uploading)

| Site | Verdict |
|---|---|
| `fuse/write_ops.rs:166` `handle_create` Dirty + cache_path | ✅ has cache_path |
| `fuse/mod.rs:828` `setattr` truncate | ✅ Fixed in beta.12 |
| `cli/commands/versions.rs:86` `restore` | ✅ Fixed here |
| `sync/upload_queue.rs:561` enters Uploading | ✅ unwraps cache_path before |
| `sync/upload_queue.rs:408` Dirty after fatal | ✅ no-op now (defensive check resets to Remote if NULL) |
| `sync/upload_queue.rs:523` Cached on non-file | ✅ dirs only |
| `state/mod.rs:771` `reset_uploading` (Uploading→Dirty) | ✅ preserves cache_path |
| `state/mod.rs` upsert CASE preserves dirty/uploading | ✅ preserves cache_path |
| `sync/conflict.rs:255` Conflict no cache_path | ✅ legitimate, invariant excludes `conflict` |

## Test-suite impact
The trigger surfaced a handful of test fixtures that encoded
`(status=Cached/Dirty/Uploading, cache_path=None)` because the
status was the part the test cared about. Fixes:

- Test helper `insert_file` in `functional.rs` now
  auto-synthesizes a placeholder cache_path when the caller passes
  None alongside one of the active statuses. Mirrors what
  production does: a row can't legitimately be in those statuses
  without a path.
- A few unit-test fixtures in `state/mod.rs::tests` and
  `integration.rs` updated to either provide a cache_path
  directly, or go through `set_dirty_with_cache_path` instead of
  `set_status(Dirty)` on a fresh remote row.
- The two intentionally-poisoned-row tests
  (`reset_stuck_dirty_files_without_cache_path_*` and
  `get_pending_uploads_excludes_files_without_cache_path`) drop
  the triggers via `raw_conn` before inserting.

## Why not just type-system invariants in Rust?
Considered: replace `set_status(Dirty)` with
`set_dirty(cache_path, size)` so the type checker forces every
caller to thread a path. Rejected — handle_write goes through a
hydrated cache_path from the FUSE FH state, which IS the same path
as the row's, but threading it back through every code path adds
ceremony for a guarantee the DB-level trigger gives for free. The
trigger fires in tests (loud) and in production (recoverable via
the read-side filter + self-heal), which is the right combination
for an invariant the codebase has a 5-month history of getting
wrong.
