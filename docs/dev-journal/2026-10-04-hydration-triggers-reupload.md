# 2026-10-04 — every hydrated file was re-uploaded

## Symptom
A read-only `rsync -av ~/stratosync/OneDrv/Pictures/ ~/Pictures/`
crawled (11 small files in ~20 min) and then hung. The journal showed
`uploading … path=Pictures/<file just read>` a minute or two after
each file was copied, followed by
`upload conflict — invoking resolver … local=<etag> remote=<id>` and
`conflict: local and remote contain identical bytes, refreshing ETag`.
OneDrive started returning `activityLimitReached`. Later the same
pattern appeared on gdrive while KDE Baloo crawled the mounts.

## Root cause
`do_hydrate` downloads to `.meta/partial/<ino>.<rand>.tmp`, renames it
to the cache path, then `set_cached`. The cache-dir watcher
(`watcher/mod.rs`) receives the rename as `Modify(Name(To))` on the
final path, looks the row up — by then `cached` — and enqueued
`UploadTrigger::Write` for any `Cached | Dirty` row. The upload's ETag
precondition then mismatched (stored etag vs. backend's form), and the
conflict resolver downloaded the remote copy a second time to compare.

Net: ~3 transfers per file read, plus spurious remote versions when an
upload did go through.

## Fix
The watcher only enqueues for `Dirty` rows. A `cached` row has no
recorded local edit; its events are the daemon's own writes. FUSE
writes set `dirty` and enqueue directly, so they don't need the watcher.

The watcher is still the trigger for out-of-process writers. The one
that exists, `stratosync versions restore`, used to copy and then mark
dirty, which could race the watcher. It now copies to a dotfile (the
watcher ignores those), marks dirty, then renames into place.

## Follow-up
Any new out-of-process writer into the cache dir must mark the row
dirty *before* the final write/rename becomes visible.
