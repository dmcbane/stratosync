# 2026-10-04 — dirty row with a missing cache file loops forever

## Symptom
Every daemon restart logged `re-queuing pending uploads count=3
mount=gdrive`, followed ~30 s later by three

```
upload fatal: backend fatal: cache file missing: ~/.cache/stratosync/gdrive/Pictures/... inode=N
```

`mount_health.consecutive_upload_failures` climbed on each restart. The
three rows (1 GB `.mov`/`.MP4` videos) had `status='dirty'`, a non-NULL
`cache_path`, and `cache_mtime` of 2026-08-28; the whole `Pictures/`
directory was gone from the cache. The remote copies were intact with
matching size and original 2015/2016 mtimes — no real local edits.

Triage query:

```sql
SELECT inode, remote_path, status, cache_path FROM file_index
WHERE kind='file' AND status IN ('dirty','uploading');
-- then check each cache_path exists on disk
```

## Root cause
Sibling of `2026-05-11-dirty-but-no-cache-path-notification-storm.md`.
That fix self-heals `cache_path IS NULL`; it did not cover a cache_path
that is set but whose file is gone. `run_upload` returned
`Fatal("cache file missing")`, the fatal handler reset the row to
`dirty`, and startup re-queued it — a permanent loop.

How the rows got dirty and lost their files on 2026-08-28 is unknown:
the journal had rotated past it. It coincided with a
`rclone stalled (no progress for 120s)` hydration failure on the same
mount. Eviction (`Cached` only) and `cache clear` (excludes dirty; last
run in June) were ruled out.

## Fix
`StateDb::revert_dirty_with_missing_cache(inode, expected_path)` —
conditional UPDATE to `remote`, clearing cache fields and the LRU row,
only while the row is still `dirty`/`uploading` **and** still names
`expected_path`. `run_upload` calls it when the cache file is missing:
reverted → `Ok` (which also resets the upload-health counter);
not reverted (a concurrent rename moved the file) → `Transient`, so the
retry reads the new path and live edits are not discarded.

## Follow-up
- Root cause of the Aug 28 dirty-marking is still open. If it recurs,
  capture the journal around the time `cache_mtime` records.
