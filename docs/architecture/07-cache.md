# Cache Management

## Cache Directory Layout

```
~/.cache/stratosync/
└── {mount_name}/              # one dir per configured mount
    ├── .meta/                 # internal metadata (not exposed via FUSE)
    │   └── partial/           # incomplete hydrations (deleted on restart)
    └── {mirrored structure}   # mirrors the remote path tree
        ├── Documents/
        │   ├── report.pdf     # hydrated file
        │   └── draft.docx     # hydrated file
        └── Photos/
            └── 2025/
                └── IMG_001.jpg
```

Cache files are regular files on the underlying filesystem. Their paths mirror the remote structure, making it easy to debug and inspect without database access.

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

## Partial / Range Hydration (Phase 2)

For large files (video, disk images), downloading the entire file before returning `open()` is unacceptable. Phase 2 adds range-based partial hydration:

```rust
// Sparse file with a range map
struct SparseCache {
    fd:        File,
    ranges:    RangeSet<u64>,   // hydrated byte ranges
    total_size: u64,
}
```

The FUSE `read(offset, size)` handler checks if `[offset, offset+size)` is in `ranges`. If not, it requests that range from the backend before serving the read.

For sequential reads (e.g., media player seeking through a video), we prefetch ahead of the current read position.

This requires backends to support range downloads (`rclone cat --offset N --count M` or HTTP Range requests via the serve-webdav mode).

---

## Pinning

Files can be pinned to prevent eviction and ensure they're always available offline:

```bash
stratosync pin ~/GoogleDrive/ImportantDoc.pdf
stratosync pin ~/GoogleDrive/WorkDocs/   # pin an entire directory
```

Pinned files have `cache_lru.pinned = 1`. They are eagerly hydrated when pinned (downloaded in background immediately).

Pin state persists across daemon restarts in the DB.

---

## Cache Integrity Checks

On daemon start, we validate the cache against the DB:
1. For every `file_index` row with `status=CACHED`, verify `cache_path` exists and `size` matches.
2. For any discrepancy, reset `status=REMOTE` and delete the cache file.
3. Scan the actual cache directory for files not in `file_index` → delete orphans.

This scan runs in the background and doesn't block daemon startup.
