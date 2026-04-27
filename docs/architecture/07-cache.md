# Cache Management

## Cache Directory Layout

```
~/.cache/stratosync/
└── {mount_name}/              # one dir per configured mount
    ├── .meta/                 # internal metadata (not exposed via FUSE)
    │   └── partial/           # incomplete hydrations (deleted on restart)
    ├── .bases/                # content-addressed object store (BaseStore)
    │   └── objects/{ab}/{cd…} # SHA-256 blobs, git-style fan-out
    └── {mirrored structure}   # mirrors the remote path tree
        ├── Documents/
        │   ├── report.pdf     # hydrated file
        │   └── draft.docx     # hydrated file
        └── Photos/
            └── 2025/
                └── IMG_001.jpg
```

Cache files are regular files on the underlying filesystem. Their paths
mirror the remote structure, making it easy to debug and inspect without
database access. The `.bases/` directory is a separate content-addressed
store — see [BaseStore](#basestore-content-addressed-blobs) below.

---

## Cache Entry Lifecycle

```
                     ┌─────────────────────────────┐
                     │  file_index.status = REMOTE  │
                     │  cache_path = NULL           │
                     └─────────────┬───────────────┘
                                   │  open() triggers hydration
                     ┌─────────────▼───────────────┐
                     │  .meta/partial/{inode}.tmp   │
                     │  Downloading from remote...  │
                     └─────────────┬───────────────┘
                                   │  download complete
                     ┌─────────────▼───────────────┐
                     │  rename(.tmp → real path)    │  ← atomic
                     │  status = CACHED             │
                     │  cache_lru.last_access = now │
                     └─────────────┬───────────────┘
                                   │
                          ┌────────┴────────┐
                          │  used normally  │
                          │  (reads/writes) │
                          └────────┬────────┘
                                   │  write() → status = DIRTY
                     ┌─────────────▼───────────────┐
                     │  Dirty file in cache         │
                     │  Modified by user            │
                     └─────────────┬───────────────┘
                                   │  upload complete
                     ┌─────────────▼───────────────┐
                     │  status = CACHED again       │
                     │  Update last_access          │
                     └─────────────┬───────────────┘
                                   │  LRU eviction trigger
                     ┌─────────────▼───────────────┐
                     │  fs::remove(cache_path)      │
                     │  cache_path = NULL           │
                     │  status = REMOTE             │
                     └─────────────────────────────┘
```

**Critical invariant**: DIRTY or UPLOADING files are never evicted. The eviction filter is:
```sql
WHERE status = 'cached' AND pinned = 0
```

---

## Quota Enforcement

Each mount has a configurable `cache_quota` (default: 5 GiB). The CacheManager monitors total cache usage and evicts LRU entries when the quota is approached.

```rust
pub struct CacheManager {
    mount_id:    u32,
    quota:       u64,
    low_mark:    f64,    // evict down to this fraction of quota (default 0.80)
    high_mark:   f64,    // start evicting at this fraction       (default 0.90)
    db:          Arc<StateDb>,
}

impl CacheManager {
    /// Called after each successful hydration. Updates the LRU position
    /// for the new entry. Does **not** trigger eviction inline — only
    /// the 60-second ticker runs `maybe_evict`. Inline eviction here
    /// used to amplify any eviction-loop dysfunction into a multi-second
    /// FUSE-mutex storm.
    pub async fn on_file_cached(&self, inode: u64, _size: u64) {
        let _ = self.db.touch_lru(inode).await;
    }

    /// Walk LRU candidates in 1000-row batches until usage drops below
    /// `low_mark * quota` or no more candidates remain. Re-querying each
    /// batch is cheap because `set_evicted` removes the row from
    /// `cache_lru`, so the same `ORDER BY last_access ASC LIMIT 1000`
    /// returns the next-oldest batch on the next iteration.
    async fn maybe_evict(&self) -> Result<()> {
        let used = self.db.total_cache_bytes(self.mount_id).await?;
        let high = (self.quota as f64 * self.high_mark) as u64;
        if used <= high { return Ok(()); }

        let target  = (self.quota as f64 * self.low_mark) as u64;
        let to_free = used.saturating_sub(target);
        let mut freed = 0u64;

        loop {
            if freed >= to_free { break; }
            let candidates = self.db.lru_eviction_candidates(
                self.mount_id, 1000,
            ).await?;
            if candidates.is_empty() { break; }

            let mut pass_freed = 0u64;
            for c in &candidates {
                if freed + pass_freed >= to_free { break; }
                pass_freed += self.evict_one(c).await?;
            }
            freed += pass_freed;
            // Bail if a whole batch made no progress (status churn);
            // the next ticker tick will retry.
            if pass_freed == 0 { break; }
        }
        Ok(())
    }

    /// Evict one entry. Returns the bytes freed (0 if skipped).
    async fn evict_one(&self, c: &EvictionCandidate) -> Result<u64> {
        // Re-check status — a write may have landed since the query.
        let cur = self.db.get_by_inode(c.inode).await?;
        if cur.map(|e| e.status) != Some(SyncStatus::Cached) { return Ok(0); }

        match tokio::fs::remove_file(&c.cache_path).await {
            Ok(()) => {
                self.db.set_evicted(c.inode).await?;
                Ok(c.cache_size)
            }
            // Phantom row: cache file already gone (manual rm, prior crash,
            // out-of-band cleanup). Treat as success — the disk space is
            // already freed; just reconcile the DB. Without this branch,
            // phantoms loop forever: the same row gets re-queried, ENOENTs
            // again, and pass_freed stays zero so the bail-out fires.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.db.set_evicted(c.inode).await?;
                Ok(c.cache_size)
            }
            Err(_) => Ok(0),  // skip this pass, retry next tick
        }
    }
}
```

**Why paginated.** A single `LIMIT 200` query stops after 200 rows; with many small files (e.g. screenshots averaging ~200 KiB) that frees only ~40 MiB per pass × 60 s ticker ≈ 2.4 GiB/h theoretical max, easily out-paced by hydration on any active mount. Paginating until usage drops to `low_mark` lets a single tick close arbitrarily large quota gaps in seconds rather than hours.

**Why ENOENT-as-success.** Cache files vanish out-of-band more often than expected: users rm to fight over-quota, the daemon crashes mid-eviction with the file removed but `set_evicted` not yet called, etc. Pre-fix, every such phantom row sat permanently in `total_cache_bytes`, was returned forever as a candidate, and on a mount with many phantoms (one observed at 1000+ in a single batch) blocked all eviction progress.

```rust
async fn evict_entry(&self, entry: &CacheEntry) -> Result<()> {
        fs::remove_file(&entry.cache_path).await?;
        self.db.set_evicted(entry.inode).await?;
        Ok(())
    }
}
```

### Eviction policy options (configurable)

| Policy | Description |
|--------|-------------|
| `lru` | Least-recently-accessed (default) |
| `lfu` | Least-frequently-used |
| `size_desc` | Evict largest files first (reclaim quota fastest) |
| `never` | Disable eviction (cache grows unbounded) |

---

## Range-Read Fast Path

Downloading an entire large file before `open()` returns is unacceptable for
media. Stratosync handles this with a **range-read fast path** during the
in-flight hydration window: when an `open()` returns immediately and the
caller starts `read()`-ing before the full background download completes,
each `read(offset, size)` call invokes `Backend::download_range(remote,
offset, size)` to fetch just the requested bytes and serve them
synchronously, while the full hydration continues in the background.

`download_range` is implemented by:

- `RcloneBackend` → `rclone cat --offset N --count M` to a temp file.
- `WebDavSidecarBackend` → HTTP `GET` with a `Range:` header.

Backends that don't support it inherit the default trait impl (returns
`SyncError::NotSupported`), and the FUSE layer falls back to blocking
the FUSE caller on the full hydration's oneshot.

The range-read fast path is what lets `vlc ~/Drive/movie.mkv` start playing
within a second instead of after the full download.

---

## Readdir Prefetch

When a directory is listed for the first time (its `dir_listed` flips from
NULL), small files in that directory are queued for background hydration.
Threshold is configurable via `[daemon.sync] prefetch_threshold` (default
1 MB; set `"0"` to disable).

Prefetch jobs run at `Background` priority — a foreground `open()` for the
same inode upgrades the in-flight hydration. The intent is to make "open
the folder, double-click the document" feel local even on a high-latency
remote.

---

## BaseStore (content-addressed blobs)

`<cache_dir>/.bases/objects/<sha256[..2]>/<sha256[2..]>` is a content-
addressed blob store, layout borrowed from git. It serves two consumers:

- **3-way merge bases** — the `base_versions` table keeps one
  `(inode, mount_id) → object_hash` row per file, used as the common
  ancestor for `git merge-file`.
- **File version history** — the `version_history` table stores N
  recent snapshots per file (per `version_retention`), captured before
  poll-driven cache replaces and after successful uploads.

Both consumers reference the same blob layout, so identical content (e.g.
an `after_upload` snapshot of an unchanged file) deduplicates to a single
blob. A blob is **only** removed from disk when no `base_versions` *and*
no `version_history` row references it. This cross-table refcount avoids
deleting a blob still serving as a merge base just because it scrolled out
of the version-retention window, and vice versa.

Stale `base_versions` rows are evicted by the cache manager on a 6-hour
cycle based on `[daemon.sync] base_retention_days` (default 30).

---

## Pinning

Files can be pinned to prevent eviction and ensure they're always available
offline:

```bash
stratosync pin ~/GoogleDrive/ImportantDoc.pdf
stratosync pin ~/GoogleDrive/WorkDocs/   # pin an entire directory, recursive
stratosync unpin ~/GoogleDrive/ImportantDoc.pdf
```

Pinned files have `cache_lru.pinned = 1`. They are eagerly hydrated when
pinned (downloaded in background immediately, so pinning a directory of 50
small files completes in parallel).

Pin state persists across daemon restarts in the DB. `stratosync status`
reports the pinned count per mount.

---

## Cache Integrity Checks

Cache↔DB drift is reconciled in **two complementary places** rather than one
omnibus startup scan:

### 1. Startup orphan reconciler (`cache::reconcile`)

Runs once per mount at daemon startup, **before the FUSE mount comes up**
(so it can't race with reads in the mount). Walks `<cache_dir>` recursively
and:

- Removes any regular file whose path isn't present in
  `file_index.cache_path` for this mount. These accumulate when a cache
  file is created out-of-band, the daemon crashes mid-cleanup, or a path
  renames without DB tracking — eviction can't reach them because it
  walks the LRU view of the DB.
- Unconditionally wipes every regular file in `.meta/partial/`. These
  are hydration temps that `do_hydrate` writes to and renames onto the
  final `cache_path` on the success path; if they exist at startup they
  were never renamed, so they're orphan by construction. (One observed
  mount had **6 stale partials totalling 14 GiB on disk** — partial
  downloads of a big file that kept being killed mid-transfer.)
- Skips `.bases/` (BaseStore-managed; has its own GC in
  `cache::evict_stale_bases`) and the rest of `.meta/` (daemon scratch).

The reserved-directory check matches direct children of the cache root
only — a user folder happening to be named `.bases` deep in their synced
tree is treated normally.

### 2. Inline ENOENT reconciliation in eviction

When `maybe_evict` tries to remove a candidate's `cache_path` and gets
`ENOENT`, the row was a "phantom" (DB row pointing at a missing file —
typically left by a prior crash or manual cleanup). We treat that as a
successful eviction: the disk space is already freed, so `set_evicted`
just clears the stale DB row. See `evict_one` above.

The two passes cover different cases:

- **Reconciler** finds files that exist on disk with no DB row at all.
  Eviction can never see these — there's no DB row to walk through, no
  LRU position to track.
- **Eviction's ENOENT branch** handles DB rows that point at missing
  files. Eviction *would* find them (they're cached-status with a
  cache_lru row) but can't free anything; pre-fix the row leaked
  forever in `total_cache_bytes`.

Together they ensure the DB-tracked cache total and the disk-resident
cache stay in sync regardless of how drift was introduced.
