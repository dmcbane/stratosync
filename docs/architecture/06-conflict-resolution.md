# Conflict Resolution

## Conflict Taxonomy

Not all conflicts are equal. We distinguish three categories:

### Type 1: Clean Remote Win
The local file is unmodified (status=CACHED) and the remote has changed.  
**Resolution**: Evict local cache, re-hydrate on next open. No conflict file created.

### Type 2: Clean Local Win  
The local file is dirty (status=DIRTY) and the remote has not changed since we last synced (ETags match).  
**Resolution**: Normal upload. No conflict.

### Type 3: True Conflict
Both local and remote have been modified since the last known-good sync point.  
**Resolution**: Both versions are preserved. This is the case that requires active handling.

---

## Detection Points

True conflicts are detected at two points:

**A. Upload time (optimistic lock failure)**  
When uploading, we check the remote ETag. If `remote_etag != known_etag`, a concurrent edit happened.

**B. Remote poll time**  
When the poller sees a changed ETag on a file we have marked DIRTY or UPLOADING, a conflict is pre-emptively detected before the upload even starts.

---

## Resolution Algorithm

```
Inputs:
  - local_path:    path to dirty local cache file
  - local_etag:    ETag of the version we based our edits on (known_etag)
  - remote_path:   canonical remote path
  - remote_etag:   current ETag on the remote
  - remote_mtime:  mtime of the remote version

Steps:

1. IDENTIFY winner
   Canonical (winning) path = remote_path (remote always wins the name)
   The remote version is what other clients see; we don't rename it.

2. BUILD conflict filename
   stem = remote_path.file_stem()       // "report"
   ext  = remote_path.extension()       // "pdf"
   ts   = now().format("%Y%m%dT%H%M%SZ") // "20250315T142301Z"
   hash = local_content_sha256()[..8]   // "a3f2e1b9"

   conflict_name = "{stem}.conflict.{ts}.{hash}.{ext}"
   // → "report.conflict.20250315T142301Z.a3f2e1b9.pdf"

   conflict_remote = parent(remote_path) + "/" + conflict_name

3. UPLOAD the local (conflicting) version under the conflict name
   backend.upload(local_path, conflict_remote, if_match=None)

4. DOWNLOAD the winning (remote) version to replace the cache
   backend.download(remote_path, local_path)
   Update file_index: etag=remote_etag, status=CACHED

5. INSERT a new file_index row for the conflict file
   (same parent_inode, kind=file, status=CACHED, name=conflict_name)

6. NOTIFY the user
   - emit a desktop notification via libnotify / notify-send
   - log to daemon journal at WARN level
   - set xattr user.stratosync.has_conflict = "1" on the parent directory
     (allows file manager integration to show conflict badge)
```

---

## Conflict Filename Design

`report.conflict.20250315T142301Z.a3f2e1b9.pdf`

Key properties:
- **Human-readable**: date/time is ISO 8601, immediately scannable.
- **Sortable**: conflicts for the same file sort together.
- **Disambiguating**: SHA256 prefix distinguishes multiple conflicts at the same second.
- **Extension preserved**: file managers open it with the right app.
- **Doesn't hide in plain sight**: unlike Dropbox's `(conflicted copy)` suffix which appears at the end, ours makes the conflict status the first thing you read after the stem.

Comparison with existing tools:

| Tool | Conflict name |
|------|---------------|
| Syncthing | `report.sync-conflict-20250315-142301-DEVICEID.pdf` |
| Nextcloud | `report (conflicted copy 2025-03-15 142301).pdf` |
| Dropbox | `report (Dale's conflicted copy 2025-03-15).pdf` |
| **stratosync** | `report.conflict.20250315T142301Z.a3f2e1b9.pdf` |

---

## Text File 3-Way Merge (Optional)

For plain text files, a 3-way merge can often resolve conflicts automatically. This requires knowing the common ancestor — the version of the file when both sides last agreed.

### Base Version Store

Base versions are stored locally in a content-addressed object store at
`~/.cache/stratosync/{mount_name}/.bases/objects/`, following git's layout
convention (`{sha256[..2]}/{sha256[2..]}`). Content-addressing gives free
deduplication.

Bases are captured at two points:
- **On hydration**: when a file is first downloaded from the remote
- **On upload**: when a file is successfully uploaded, the uploaded content becomes the new base

Only text-mergeable files get base snapshots (gated by extension allowlist,
size threshold ≤ `base_max_file_size`, and NUL-byte binary detection).

The `base_versions` table in StateDb maps `(inode, mount_id)` → `object_hash`.
Stale entries are evicted every 6 hours based on `base_retention_days`.

```toml
[sync]
text_conflict_strategy = "merge"   # default: "keep_both"
text_extensions = ["md", "txt", "rs", "py", "toml", "yaml", "json"]
base_retention_days = 30           # how long to keep base objects
base_max_file_size = "10 MB"       # skip base capture for larger files
```

### Merge Algorithm

When `text_conflict_strategy = "merge"` and a base version exists:

```
base   = content-addressed blob from .bases/objects/
local  = current dirty cache file
remote = downloaded from backend to temp file

result = git merge-file --stdout local base remote

exit 0 (clean merge):
    write merged content to canonical path
    upload merged result (no ETag check)
    update base version to merged result
    no conflict file created

exit 1 (conflict markers):
    write merged-with-markers to canonical path
    create .conflict.* sibling with original local version
    notify user: "Partial merge — review conflict markers"

other/failed/git not found:
    fall back to keep_both (existing behavior)
```

We shell out to `git merge-file` — it is the gold standard for 3-way text
merge, available on virtually all Linux systems, and produces standard
`<<<<<<<` / `=======` / `>>>>>>>` conflict markers. If `git` is not
installed, merge is silently skipped and the keep-both strategy applies.

---

## Directory Conflicts

Directory-level conflicts (e.g., same directory deleted remotely while files were added locally) are handled conservatively:

1. Remote delete of a directory that has local `DIRTY` files → abort the remote delete, keep the directory, log a warning.
2. Local rename of a directory while remote renamed the same directory → local rename wins for paths we own; add both to file_index; flag for user review.

---

## Conflict Resolution CLI

The `stratosync conflicts` command lists all conflict files, and four subcommands resolve them:

```
$ stratosync conflicts
Mount: gdrive
────────────────────────────────────────────────────────────
  CONFLICT  report.pdf
            inode=42  size=12 KB  modified=2025-03-15 14:23 UTC
            remote: Documents/report.pdf

$ stratosync conflicts keep-local  ~/GoogleDrive/Documents/report.pdf
$ stratosync conflicts keep-remote ~/GoogleDrive/Documents/report.pdf
$ stratosync conflicts merge       ~/GoogleDrive/Documents/report.pdf
$ stratosync conflicts diff        ~/GoogleDrive/Documents/report.pdf
```

- **keep-local**: uploads the local cached version, deletes the `.conflict.*` sibling
- **keep-remote**: downloads the remote version, deletes the `.conflict.*` sibling
- **merge**: attempts 3-way merge via `git merge-file` using the base version store; on conflict markers, writes them to the file for manual editing
- **diff**: shows a unified diff between local and remote versions
