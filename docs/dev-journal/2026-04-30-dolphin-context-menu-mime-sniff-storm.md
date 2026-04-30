# Dolphin Context-Menu Freezes on Folders of Large Files

**Date:** 2026-04-30
**Context:** v0.12.1 Dolphin/Nautilus interop pass — right-clicking 11 selected MP4s (~1–2 GB each, status=`Remote`) on a Google Drive mount froze Dolphin for 30–60 s before the menu appeared. Live-debugged against the real backend with a fresh debug build.
**Related Commits:** `cf65e2d` (initial header cache), `d030e25` (deferred auto-hydrate), `964847a` (focus-aware prefetch + async-spawn read)
**Status:** RESOLVED

## Problem

Right-clicking N selected files in Dolphin took ≈ 3 s × N to produce a context menu. On a folder of media files where each one is multi-GB, that's a 30–60 s UI freeze. Worse: each right-click queued a multi-GB `rclone copyto` in the background, so a casual right-click on five videos kicked off five full-file downloads the user never asked for.

The first three iterations of fixes each helped a little (45 s → 23 s → 23 s) but didn't get to "instant." The actual fix wasn't in any of the obvious places.

## Investigation

### Initial hypothesis (wrong-ish): "MIME sniffing reads, just cache the headers"

KDE/KIO sniffs MIME types and generates thumbnails by `open()` + `read(offset=0, len=16-64 KiB)`. Each of those reads on a `Remote` file used to be a fresh `rclone cat` invocation — process spawn + OAuth refresh + cloud round-trip ≈ 1–3 s. Multiply by the selection count and you get the freeze.

The first attempt was a header cache: pre-fetch the first 64 KiB of every file in a directory, serve sniff reads from local disk. Triggered on first `populate_directory` of a directory. New config knobs `[sync] header_prefetch_size` and `[sync] auto_hydrate_max_size` (skip the legacy open()-time `rclone copyto` for files over 100 MB).

Plus an `auto_hydrate_max_size` flag so KIO sniffing huge MP4s wouldn't kick off multi-GB background downloads.

This *should* have made everything fast. It didn't. Right-click was still 23 s after a "warm-up" wait.

### What the log actually showed

After `RUST_LOG=stratosync=debug` and reproducing live, two things stood out.

**1. The reads were running serially despite parallel-capable code.** Eleven sniff reads fired roughly 3 seconds apart:

```
18:39:01.083  cat Church242_20251005.mp4 --offset 0 --count 32768
18:39:04.185  cat Church242_20251221.mp4 --offset 0 --count 32768
18:39:07.359  cat Church242_20250928.mp4 --offset 0 --count 32768
...
```

3 s gaps == the duration of one rclone-cat. Even though our async runtime had plenty of workers, every read was waiting for the previous one to complete.

**2. Headers existed for the wrong files.** 644 header files on disk, zero of them for the inodes Dolphin actually right-clicked.

### Two surprises behind those observations

**Surprise 1: KIO's MIME-sniff loop is serial.** Dolphin's `KMimeTypeDatabase::mimeTypeForFile` runs file-by-file: open, read, await reply, close, next file. It doesn't pipeline. So no amount of parallelism on the FUSE side shortens the wall-clock for a multi-file right-click — *unless* each individual read returns instantly. The only way to make N-file right-click feel instant is to have all N headers already on disk when KIO starts asking.

**Surprise 2: `rt.block_on()` in the FUSE handler wasn't even letting the *daemon* parallelize what little it could.** fuser's session loop reads requests one at a time and dispatches synchronously. Our `read()` handler did `self.rt.block_on(async move { … })` — which blocks the FUSE worker thread until the work finishes, holding up the next request from the kernel even when the kernel has already pipelined it. Switching to `self.rt.spawn(…)` and replying from inside the spawned task lets the FUSE worker accept the next request immediately. This still doesn't help against KIO's serial caller, but it unlocked parallelism for any caller that does pipeline (cp -r, rsync, parallel readdir of many files, etc.).

**Surprise 3 — the actual fix: prefetch queue starvation.** Even after re-enabling prefetch on every `readdir`, the user's right-clicked folder *was* getting prefetched — at `20:02:17`, **3 minutes 35 seconds after they clicked**. Why? Dolphin's tree-view + breadcrumb panel was readdir-ing 30+ unrelated directories during their browse session, each one queuing prefetches for *its* files. The shared prefetch semaphore (initially 8 concurrent) drained the queue FIFO. By the time the user's actual target reached the front, they'd given up waiting.

The fix that finally worked was making the prefetch queue *focus-aware*: each `readdir` records its inode in `prefetch_focus_dir`, and queued prefetch tasks check it after waking from the semaphore. Tasks for stale (no-longer-focused) directories bail out without doing the rclone-cat. So when the user navigates to /A then /B then /C, the queued prefetches for /A and /B silently abandon themselves and /C gets the full bandwidth budget.

## Solution

Five interlocking changes (commit `964847a`):

- **`prefetch_inflight: Arc<DashMap<Inode, ()>>`** on `StratoFs`. Concurrent readdirs and concurrent reads consult it before spawning a fetch — no duplicates ever.
- **Daemon-wide `prefetch_sem: Semaphore(16)`**. Per-call semaphores were doing nothing to bound total fanout across many readdirs.
- **`prefetch_focus_dir: AtomicU64`**. Stale prefetch tasks bail when they wake.
- **Async-spawn in `read()`**: `rt.spawn` instead of `rt.block_on`, reply from inside the task. Frees the FUSE worker thread.
- **Opportunistic-cache no longer fetches more than what was just read**: prior version was issuing an extra `rclone cat` to fill out to `header_size` if the sniff was smaller, doubling cold-sniff bandwidth for no win.

## Key Learnings

- **Profile the *caller's* dispatch pattern, not just your own.** "Make our side parallel" is a reflexive optimization for I/O-bound work, but it does nothing when the upstream serializes. KIO's MIME sniffer is single-threaded by design; the only way to improve N-file right-click latency is to drop per-file latency to ≈ 0. We chased "make the daemon parallel" through three iterations before measuring that the gap between sequential cats was exactly one cat-duration.
- **`rt.block_on` in a FUSE handler is a hidden serializer.** Even if every other layer is async, a single `block_on` in the synchronous FUSE callback turns the whole pipeline back into a single-threaded queue. Use `rt.spawn` and send the reply from the spawned task. `ReplyData` and friends are `Send`.
- **Background queue depth grows monotonically with user navigation.** Any "prefetch on every readdir" scheme needs either bounded queue depth or focus-aware abandonment. Otherwise the user's *current* target lands at the back of a multi-minute queue of stale work from sidebar dirs they walked past hours ago.
- **`tokio::sync::Semaphore` doesn't free queued tasks when the semaphore caller goes away.** Tasks that have already called `acquire().await` and are queued for a permit will *still* eventually wake up and do their work unless they explicitly check whether they're still relevant. We use an `AtomicU64` focus marker that the task re-reads after acquiring its permit.
- **Live debugging in the auto-mode "fix-and-rebuild" loop beat reasoning ahead of time.** Each of the first three rebuilds was based on a plausible-sounding theory that turned out to be missing the actual bottleneck. Running the actual daemon with `RUST_LOG=debug`, having the user reproduce, and reading timestamps in the log was what surfaced the focus-starvation issue. Without the timestamp gap analysis we would have shipped "23 s on a primed folder" thinking we were done.
- **`rclone` CLI per-call latency is ≈ 1–3 s.** Each `rclone cat --offset N --count M` invocation has a fixed cost (process spawn + OAuth refresh-check + cloud RTT) that dominates for small reads. For high-frequency small reads, a long-lived rclone process via `rclone rcd` or `rclone serve restic` would be the next-tier optimization — same range request hits ≈ 50 ms instead of 2 s. We didn't take that step because the header cache + focus-aware prefetch reduced the *count* of cat calls enough, but it remains the obvious lever if someone wants further wins for non-prefetchable workloads (random-offset reads, video playback while not yet hydrated).

## Verification

Live test, fresh empty header cache, real Google Drive, eleven 1–2 GB MP4s in `gdrive:/Documents/Church242/Recordings/Videos/Processed/`:

| Run | Behavior | Result |
|---|---|---|
| Pre-fix (v0.12.0) | Right-click w/o any of the new code | 45 s freeze; 11 serial 3 s cats; 11 multi-GB rclone-copyto in background |
| Run 4 | Header cache + auto-hydrate skip + prefetch on every readdir | 23 s; cat-storm of 5,532 invocations across 75 min of background prefetch queue |
| Run 5 | + dedup (inflight DashMap, on-disk re-check) | 23 s; clean log but Processed/ still at the back of a 3.75-min queue |
| Run 6 | + focus-aware abandonment + concurrency 16 + async-spawn read | **Instant** after navigating directly into the folder. 104 cat calls total; 0 copyto calls; 16 Processed/ MP4 prefetches all fired within the same millisecond after focus landed on the dir |

## Related Files

- `crates/stratosync-daemon/src/fuse/mod.rs` — `StratoFs` fields (`prefetch_inflight`, `prefetch_sem`, `prefetch_focus_dir`), `spawn_prefetch_headers`, `read()` handler refactor
- `crates/stratosync-daemon/src/fuse/header_cache.rs` — on-disk header storage with atomic temp-rename, owner-only perms
- `crates/stratosync-core/src/config/mod.rs` — `header_prefetch_size`, `auto_hydrate_max_size` knobs
- Commits: `cf65e2d`, `2f68083`, `5f5d53e`, `d030e25`, `964847a` (full v0.12.1 Dolphin/Nautilus interop pass)
