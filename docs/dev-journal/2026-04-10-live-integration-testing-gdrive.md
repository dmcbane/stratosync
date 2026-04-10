# Live Integration Testing: Google Drive 3-Way Merge and File Operations

**Date:** 2026-04-10
**Context:** End-to-end testing of 3-way merge conflict resolution, file/directory operations, and upload queue against `gdrvtest:` Google Drive remote
**Related Commits:** 24dc565 (ETag hash fix), ad3f947 (watcher fix), 2907fe0 (upload queue redesign), cb48120 (abort race fix)

## Problem

Needed to verify the full conflict resolution pipeline works against a real Google Drive backend, including:
- ETag-based conflict detection
- 3-way merge via `git merge-file`
- Base version capture and retrieval
- Keep-both fallback for binary files
- All FUSE file/directory operations

## Investigation

### Phase 1: File/Directory Operations (10/10 pass)

All basic FUSE operations work correctly against Google Drive:

| Operation | Status |
|-----------|--------|
| File read (hydration) | PASS |
| File write + upload | PASS |
| Create new file | PASS |
| mkdir | PASS |
| Create file in new dir | PASS |
| Rename file | PASS |
| Delete file | PASS |
| Delete directory (rm -rf) | PASS |
| Nested file write + upload | PASS |
| Binary conflict (keep-both) | PASS |

### Phase 2: Conflict Detection Failures

Initial conflict tests all failed — uploads succeeded without detecting the remote change. Root cause investigation:

**Bug 1: `stat()` didn't use `--hash`**

`RcloneBackend::stat()` called `lsjson --no-modtime=false` without `--hash`. Without `--hash`, rclone returns no content hashes in the `Hashes` field. The ETag extraction code fell back to the Google Drive file ID (`e.id.clone()`). File IDs don't change when content is updated (`rclone rcat` modifies content in-place, keeping the same ID), so the ETag comparison always matched.

**Bug 2: Hash key case mismatch**

Even after adding `--hash` to `stat()`, ETags were still file IDs. The hash extraction code looked for `"SHA-1"` and `"MD5"` (uppercase), but rclone outputs `"sha1"` and `"md5"` (lowercase) inside the `Hashes` JSON object. The `HashMap::get()` is case-sensitive, so the lookup always failed and fell back to the file ID.

**Verification:**
```
$ rclone lsjson --hash gdrvtest:/ss-test/file.txt
[{"Hashes": {"md5": "fa74...", "sha1": "bd7e..."}, "ID": "1PKD..."}]
                     ^^^^lowercase         ^^^^lowercase
```

### Phase 3: After ETag Fixes

With both fixes applied, conflict detection worked:

```
upload conflict — invoking resolver  inode=20  local=Some("1SBR...") remote=Some("f76c...")
3-way merge clean — no conflict file needed  inode=20  path=ss-test/merge-conflict.txt

upload conflict — invoking resolver  inode=18  local=Some("1pDf...") remote=Some("df7f...")
3-way merge clean — no conflict file needed  inode=18  path=ss-test/code.rs

upload conflict — invoking resolver  inode=19  local=Some("1awA...") remote=Some("ee40...")
3-way merge clean — no conflict file needed  inode=19  path=ss-test/merge-clean.txt
```

All 3 test files: conflict detected → 3-way merge invoked → clean merge → merged result uploaded → no conflict sibling needed.

Note: the first set of conflicts were triggered by the ETag type mismatch (old file ID in DB vs new SHA-1 from `stat --hash`). This is a one-time transition — after the merged result is uploaded, the DB stores SHA-1 hashes and all subsequent comparisons are hash-vs-hash.

## Solution

**Commit 24dc565:**

1. Added `--hash` to `stat()` in `RcloneBackend`:
```rust
// Before:
let bytes = self.run(&["lsjson", "--no-modtime=false", &rp]).await?;
// After:
let bytes = self.run(&["lsjson", "--no-modtime=false", "--hash", &rp]).await?;
```

2. Fixed hash key case in `RemoteMetadata::try_from()`:
```rust
// Before:
h.get("SHA-1").or_else(|| h.get("MD5"))
// After:
h.get("sha1").or_else(|| h.get("SHA-1"))
    .or_else(|| h.get("md5")).or_else(|| h.get("MD5"))
```

## Key Learnings

- **Google Drive file IDs are NOT content-based**: they persist across content changes. Using file IDs as ETags gives false "no conflict" results. Only content hashes (MD5/SHA-1) detect actual content changes.
- **rclone's `lsjson` requires `--hash` explicitly**: without it, the `Hashes` field is empty/absent. The `ID` field is always present as a fallback, which masks the missing hashes.
- **rclone uses lowercase hash names internally**: `md5`, `sha1`, `sha256` — despite the outer JSON using PascalCase (`Hashes`, `MimeType`, etc.). The hash names inside the `Hashes` object are provider-specific strings, not serde-renamed.
- **Live testing timing is critical**: the test script's edits, remote modifications, and upload waits must be carefully sequenced. The daemon's asynchronous upload queue + debounce + conflict resolution pipeline makes deterministic test scripting difficult — verifying from daemon logs is more reliable than checking file contents at a point in time.
- **OneDrive has a separate rclone path format issue**: `lsjson --recursive` returns paths like `drives/4139BC84D0721EB5/root:/ss-test/file.txt` instead of `ss-test/file.txt`. This breaks directory listing and is unrelated to merge/conflict logic.

## Tags

`#integration-testing` `#google-drive` `#etag` `#conflict-detection` `#3-way-merge` `#rclone`
