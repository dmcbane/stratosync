# Upload Queue Drops Subsequent FUSE Writes

**Date:** 2026-04-10
**Context:** Live testing 3-way merge conflict resolution against `gdrvtest:` Google Drive remote
**Related Commits:** (investigation in progress)
**Status:** OPEN BUG

## Problem

Files written through the FUSE mount get marked `dirty` in the DB, but the upload queue silently ignores them. The first batch of dirty files (from the initial edit cycle right after mount) uploads correctly, but subsequent writes to the same or different files never trigger an upload.

### Reproduction

1. Start daemon with a Google Drive mount
2. Wait for initial sync (files appear in mount)
3. Read a file through the mount (hydrates it)
4. Edit the file (e.g., `sed -i 's/old/new/' mount/file.txt`)
5. The DB shows `status = dirty` for the file
6. **Expected**: upload queue picks it up within debounce period and uploads
7. **Actual**: file stays `dirty` forever; no upload log entries appear

### Observed Behavior

- Inodes 15, 16, 17 (code-merge.rs, conflict-markers.txt, large-text.md) uploaded fine during the first batch
- Inode 14 (clean-merge.txt) was dirty from the same batch but never uploaded
- Subsequent edits to inode 15 (code-merge.rs) after it was cached never triggered a re-upload
- The upload queue's `uploading` log messages only appear for the first batch
- `fsync()` calls from Python also don't trigger the upload

### Key Observations

- The FUSE `write()` handler calls `queue.enqueue(UploadTrigger::Write { inode })` (write_ops.rs:84)
- The FUSE `release()` handler calls `queue.enqueue(UploadTrigger::Close { inode })` (write_ops.rs:113)
- The FUSE handlers run on a `std::thread` and use `Handle::block_on()` to call async code
- The upload queue uses a `tokio::sync::mpsc` channel to receive triggers
- The inotify watcher on the cache directory may also trigger uploads

## Investigation

The upload queue uses a `tokio::select!` loop with two arms:
1. `rx.recv()` — inbound triggers from FUSE writes
2. `in_flight.join_next()` — completed upload tasks

Each inode gets a debounce timer task in `in_flight`. A `pending` HashMap maps `inode → oneshot::Sender<()>` to allow aborting the debounce if a new write arrives for the same inode.

### Root Cause: Abort Completion Removes Wrong Pending Entry

The race condition:

1. Trigger for inode X → task A spawned, `abort_tx_A` stored in `pending[X]`
2. **Second trigger for X** → `abort_tx_A` removed and sent (aborts A), `abort_tx_B` stored in `pending[X]`, task B spawned
3. **Task A completes** (aborted, returns `Ok(())`) → completion handler calls `pending.remove(X)` — **removes `abort_tx_B`** (the wrong entry!)
4. `abort_tx_B` is dropped → `abort_rx_B` resolves immediately (oneshot resolves on sender drop) → **task B aborts without uploading**
5. Inode X is never uploaded

The key insight: the oneshot `abort_rx` in `tokio::select!` matches when the sender is dropped — not just when `send()` is called. Dropping `abort_tx_B` by removing it from the HashMap is equivalent to aborting task B.

### Why First Batch Works

The first batch of files uploads correctly because they are triggered at different times (hydration + initial writes) and don't overlap — each inode goes through a clean trigger → debounce → upload cycle. The bug only manifests when:
- Multiple triggers fire for the same inode (write then close), OR
- An aborted task's completion races with the replacement task's pending entry

## Solution

**File**: `crates/stratosync-daemon/src/sync/upload_queue.rs`

Added a `bool` flag to the `JoinSet` return type: `(Inode, Result<(), SyncError>, bool)` where `true` = aborted.

- Aborted tasks return `(inode, Err(...), true)` 
- The completion handler skips `pending.remove()` for aborted tasks
- Real completions (upload success, conflict, error) still remove from pending as before

```rust
// Before (broken):
in_flight.spawn(async move {
    tokio::select! {
        _ = sleep(window) => {}
        _ = abort_rx      => { return (inode, Ok(())); }
    }
    ...
});
// Completion: pending.remove(&inode) — removes WRONG entry

// After (fixed):
in_flight.spawn(async move {
    tokio::select! {
        _ = sleep(window) => {}
        _ = abort_rx      => { return (inode, Err(...), true); }
    }
    ...
    (inode, result, false)
});
// Completion: skip pending.remove for aborted tasks
```

## Key Learnings

- `tokio::sync::oneshot::Receiver` resolves when the sender is **dropped**, not just when `send()` is called. This makes it unsuitable as a "cancel token" if the sender's lifetime isn't carefully managed.
- When using a HashMap to track in-flight operations by key, completion handlers must verify they're removing the correct entry — not a replacement that was inserted between spawn and completion.
- `tokio::select!` makes timing-dependent bugs easy to introduce: the two arms can interleave in unexpected ways, especially when one arm's completion modifies state that the other arm depends on.

## Tags

`#upload-queue` `#tokio` `#race-condition` `#oneshot` `#select`
