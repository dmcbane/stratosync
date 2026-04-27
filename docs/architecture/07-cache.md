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
    cache_dir:   PathBuf,
    quota_bytes: u64,
    db:          Arc<StateDb>,
}

impl CacheManager {
    /// Called after each successful hydration.
    pub async fn on_file_cached(&self, inode: u64, size: u64) {
        self.db.update_cache_size(inode, size).await;
        self.db.touch_lru(inode).await;
        self.maybe_evict().await;
    }

    /// Evict until total cache usage is below 90% of quota.
    async fn maybe_evict(&self) {
        let total = self.db.total_cache_size(self.mount_id).await;
        if total < self.quota_bytes * 9 / 10 {
            return;
        }
        let target = self.quota_bytes * 8 / 10;  // evict down to 80%
        let candidates = self.db.lru_eviction_candidates(self.mount_id).await;
        let mut freed = 0u64;
        for entry in candidates {
            if total - freed <= target { break; }
            if let Err(e) = self.evict_entry(&entry).await {
                warn!("Failed to evict {}: {}", entry.cache_path, e);
            } else {
                freed += entry.cache_size;
            }
        }
    }

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

On daemon start, we validate the cache against the DB:
1. For every `file_index` row with `status=CACHED`, verify `cache_path` exists and `size` matches.
2. For any discrepancy, reset `status=REMOTE` and delete the cache file.
3. Scan the actual cache directory for files not in `file_index` → delete orphans.

This scan runs in the background and doesn't block daemon startup.
