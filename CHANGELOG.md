# Changelog

All notable changes to this project will be documented in this file.

## [0.13.0-beta.14] - 2026-05-11

### Fixed
- **Dashboard halted with stuck "N consecutive failures" after the
  beta.13 cleanup**. Migration 0010 (and the runtime sweep) deleted
  the poisoned `file_index` rows but left
  `mount_health.consecutive_upload_failures` alone — the counter
  only decrements on `record_upload_success`, and post-cleanup no
  upload even gets attempted (rows are back to `remote`). Result:
  the dashboard kept reporting "554 consecutive failures — backend
  fatal: inode N: dirty but no cache_path" against a mount that
  no longer had anything to fail on.

  Two-part fix:
  - **Migration 0011** retroactively clears `mount_health` rows
    whose `last_upload_error LIKE '%dirty but no cache_path%'`.
    Pattern-match scope is exactly the bug string from pre-beta.13
    `run_upload`'s fatal branch, so legitimate transient errors
    (rclone timeout, auth failures) keep their counters. Catches
    every population: direct upgraders from beta.11, beta.12
    intermediates whose rows beta.12's runtime sweep ate before
    0010 saw them, and users already on beta.13 with stuck banners.
  - **Runtime sweep** (`reset_stuck_dirty_files_without_cache_path`)
    now clears upload-health for affected mounts in the same atomic
    update as the row cleanup. Defense in depth: if a regression
    ever reproduces the bug shape, both layers heal at once.

## [0.13.0-beta.13] - 2026-05-11

### Added
- **Schema invariant for file rows.** Migration 0010 installs two
  BEFORE INSERT/UPDATE triggers that make
  `kind='file' AND status IN ('cached','dirty','uploading') AND
  cache_path IS NULL` physically impossible. The same migration
  runs a cleanup pass that reverts any existing rows in this state
  to `remote`, so the move is safe over pre-fix data. `stale` and
  `conflict` are exempt — the poller legitimately flips never-
  hydrated rows to `stale`, and conflict siblings can live
  remote-only.

### Fixed
- **`versions restore` could poison a never-hydrated row** with the
  same "dirty but no cache_path" shape that beta.12 fixed for
  `setattr`. The restore path synthesized a cache_path and wrote
  the blob to it, then called `set_status(Dirty)` without
  recording the synthesized path on the row. Now uses
  `set_dirty_with_cache_path` (and would be caught by the
  migration-0010 trigger if regressed).
- **`get_pending_uploads` filters out `cache_path IS NULL` rows**
  at the source. Read-side companion to the trigger: even if some
  future code path slips an invariant violation past the DB
  (legacy data, sqlite3 shell edits), the upload queue physically
  cannot see it.

## [0.13.0-beta.12] - 2026-05-11

### Fixed
- **Notification storm: "upload failed — dirty but no cache_path"**
  resolved at the root. `setattr(size=N)` (and `open(O_TRUNC)`, which
  the kernel translates to setattr) on a never-hydrated file used to
  materialize a cache file on disk but call `set_dirty_size`, which
  flips the row to `status='dirty'` *without* recording the
  synthesized cache_path. The upload queue then loaded the row, hit
  `cache_path = NULL`, returned `Fatal("dirty but no cache_path")`,
  and the fatal handler set the row back to `dirty` — poisoning it
  forever and firing a desktop notification on every retry. Joplin
  saves (O_TRUNC-rewrite-then-write) and other "edit a never-opened
  remote file" workflows were the dominant trigger; observed in the
  wild as a 277-row backlog spamming notifications on every daemon
  restart.

  Three-part fix:
  - New `StateDb::set_dirty_with_cache_path` records status, size,
    and cache_path in one update. `setattr`'s "create cache file
    out-of-band" branch now calls this instead of `set_dirty_size`.
  - New `StateDb::reset_stuck_dirty_files_without_cache_path`
    startup sweep (symmetric to the directory cleanup added earlier):
    rows with `kind='file' AND status IN ('dirty','uploading') AND
    cache_path IS NULL` revert to `status='remote'`, so the next
    open() re-hydrates from the cloud. Heals the existing backlog
    on first restart after upgrade.
  - Defense in depth in `run_upload`: if it ever sees a dirty row
    with no cache_path again (some new code path regressing into the
    same shape), it self-heals to `remote` and returns Ok instead
    of looping through the fatal handler with a notification per
    cycle.

## [0.13.0-beta.11] - 2026-05-09

### Added
- **`transfer_window` config field** — bandwidth scheduling for the
  download side, the third item from the upload/download symmetry
  audit. Previously only uploads could be gated by a daily window;
  background prefetch ran any time, which was unhelpful for users on
  metered connections who wanted "no big downloads during work hours."

  ```toml
  [[mount]]
  transfer_window = "22:00-06:00"             # both sides by default
  transfer_window_direction = "download"      # or "upload" / "both"
  ```

  - `TransferDirection` enum with `lowercase` serde names. Default is
    `both`, so a bare `transfer_window` without a direction does what
    most users mean ("only sync at night").
  - Gating semantics on the download side mirror the upload side:
    speculative work (`spawn_prefetch_small_files`,
    `spawn_prefetch_headers`) skips when the window is closed, while
    user-initiated `open()` always proceeds — same shape as how
    `fsync` always bypasses the upload window.
  - Legacy `upload_window` continues to work; equivalent to
    `transfer_window` with `direction = "upload"`. Setting both
    fields together is a configuration error with a message that
    names both and explains the relationship.
  - 8 new tests in `core::config` covering: default-is-both,
    explicit-download, explicit-upload, legacy-upload-window
    back-compat, mutual-exclusion error, empty-string-as-unset,
    direction-without-window-is-a-no-op, and the all-empty case.

### Changed
- `UploadWindow` renamed to `TransferWindow` — the type was always
  just a daily HH:MM-HH:MM interval and was never upload-specific.
  No external impact (only used inside the workspace);
  `parse_upload_window` → `parse_transfer_window`. The lib re-export
  switched from `pub use config::UploadWindow` to
  `pub use config::{TransferDirection, TransferWindow}`.

## [0.13.0-beta.10] - 2026-05-09

### Changed
- **Hydration tracker reuses `SyncError::is_retryable()` for the
  terminal-vs-retryable decision** instead of a hand-rolled match.
  Keeps the upload queue and the hydration tracker in lockstep if the
  retryability predicate ever changes. Behavior is the same for the
  cases the existing tests cover (success, Transient, stale-path
  prune); slight shift for three error variants which are now treated
  as terminal (was: preserved across retries):
  - `Io`: local disk/FS errors aren't going to recover by retrying
    the cloud, so clearing the tracker is the right default.
  - `NotSupported`: a programming bug; terminal is correct.
  - `QuotaExceeded`: now preserved across retries (`is_retryable`
    returns true). The dashboard's attempt counter will rise on
    repeated quota errors instead of resetting each time, which is
    more informative.
  The `NotFound` case retains its carve-out (terminal iff the row
  was pruned by the stale-path self-heal).

## [0.13.0-beta.9] - 2026-05-09

### Added
- **`stratosync push <path>` — force-upload now.** Mirror of `pin` for
  the upload direction, second item from the symmetry audit. Triggered
  by the question "if my editor saved a file but it's stuck in a 60s
  retry-backoff window and I'm about to close my laptop, how do I make
  it upload right now?" Previously the only mechanism was an editor's
  built-in `fsync()` (and not all editors expose one).
  - Single-file form: `stratosync push my-doc.md` opens the file
    through the FUSE mount and calls `sync_all()`. The kernel forwards
    the fsync to the daemon's FUSE callback, which already enqueues
    `UploadTrigger::Fsync` (window=ZERO, immediate=true) — so all the
    bypass machinery already lives in the daemon and we don't need a
    new IPC route. No daemon-side changes required.
  - Directory form: `stratosync push <dir>` recurses (matching `pin`'s
    behavior) and pushes every Dirty/Uploading descendant; already-
    synced files are silently skipped. Refused files (Remote /
    Hydrating / Stale / Conflict) are reported in a tail block.
  - Status-classification matrix:
    - `Dirty`/`Uploading` → fsync (the whole point).
    - `Cached` → friendly "already synced" message; exit 0.
    - `Remote`/`Hydrating` → refuse: "file is not downloaded locally".
    - `Stale` → refuse: pushing the older local copy would clobber
      remote changes.
    - `Conflict` → refuse: point at `stratosync conflicts`.

## [0.13.0-beta.8] - 2026-05-09

### Added
- **Upload health symmetry.** The `mount_health` table and surrounding
  code paths previously tracked only hydration (download) failures —
  the tray flipped its warning icon on download retry loops but stayed
  silent during a 41-attempt upload retry, and Prometheus had no upload
  failure gauge. Closes the most concrete asymmetry surfaced by the
  audit:
  - Migration `0009_mount_health_upload.sql` adds three columns
    (`consecutive_upload_failures`, `last_upload_error`,
    `last_upload_failure_unix`).
  - `record_upload_failure` / `record_upload_success` mirror the
    hydration pair (same upsert pattern, same "leave last_error intact
    after success" semantics).
  - The upload queue calls them on every terminal outcome — success,
    conflict, retryable error, fatal error — matching what
    `do_hydrate` already does for downloads.
  - `QueueStatus` IPC payload gains `consecutive_failures`,
    `last_error`, `last_failure_unix` (all `#[serde(default)]` for
    legacy-daemon tolerance during partial-upgrade rollouts).
  - New Prometheus gauge `stratosync_mount_upload_consecutive_failures`
    (twin of the existing `…_hydration_consecutive_failures`).
  - The dashboard `status` column now factors uploads into its
    worst-of decision; the poller block prints `uploads: N consecutive
    fail(s) — err` when non-zero.
  - The tray's warning icon trips on either side stalling. The tooltip
    distinguishes `upload`, `download`, or `transfers` (when both),
    and the menu shows one line per stalled direction so a mount
    stalled on both surfaces both errors.
  - 5 new state-DB tests + 4 new tray tests covering threshold
    behavior, direction independence, the both-sides "transfers"
    label, and the IPC backward-compat round-trip.

## [0.13.0-beta.7] - 2026-05-09

### Fixed
- **In-flight progress now actually populates in the dashboard.** Two
  bugs shipped together in beta.5 (uploads) and beta.6 (downloads)
  prevented the live byte counter from ever leaving zero in
  production:
  1. `--stats-log-level=NOTICE` was suppressed by the daemon's master
     `--log-level=ERROR`. rclone generated stats lines at NOTICE but
     the master filter dropped them on the floor before they reached
     our parser. Switched both `upload_with_progress` and
     `download_with_progress` to `--stats-log-level=ERROR` so the
     master filter lets them through.
  2. The progress parser searched for the literal substring
     `"Transferred:"`, which never appears when rclone is invoked with
     `--use-json-log` (we always set this in `extra_flags`). rclone's
     JSON envelope strips the `Transferred:` prefix and exposes the
     value as `stats.bytes`. Updated the parser to read `stats.bytes`
     directly when the line is JSON, falling back to the original
     plain-text logic for non-JSON-log mode.

  Verified end-to-end on a live OneDrive mount: a 825 MB upload now
  shows `220.2 MB/825.3 MB 26% 32s` and climbs in real time
  (~8.7 MB/s), where beta.5/beta.6 stayed at `0 B / 825.3 MB 0%`
  for the entire upload regardless of actual progress.

## [0.13.0-beta.6] - 2026-05-09

### Added
- **Dashboard in-flight panel now also shows hydrations.** Direct
  twin of the upload panel introduced in beta.5: each row shows path,
  bytes-downloaded/total, current-attempt elapsed, and (when > 1)
  attempt count plus first-seen elapsed. Triggered by the user
  reporting "I copied 20 files from a OneDrive mount, 14 succeeded,
  and now it appears frozen" — the dashboard had no per-file
  visibility on the download side, so a single stalled `cp` was
  indistinguishable from a wedged daemon. The TUI in-flight section
  now always renders both `uploads:` and `hydrations:` headings (with
  `(none)` when empty) so the user can confirm "yes, this download
  is in flight" or "no, nothing is moving — investigate elsewhere."
- `Backend::download_with_progress` mirroring `upload_with_progress`
  (default-implemented to fall back to plain `download`).
  `RcloneBackend` overrides with the same `--stats=1s
  --stats-one-line --stats-log-level=NOTICE` pipeline used by uploads.
- `HydrationStatus.in_flight: Vec<ActiveHydration>` on the IPC
  payload. Defaults to empty for legacy daemons during partial-
  upgrade rollouts.
- `ActiveHydration` IPC type with the same retry-aware fields as
  `ActiveUpload` (`first_started_unix`, `attempt`,
  `bytes_downloaded`).
- `fuse::HydrationTracker` — three `Arc<DashMap>`s shared between
  `do_hydrate` and the dashboard snapshot path. `in_flight` clears
  on every exit (success or failure); `first_started` and `attempts`
  survive retryable failures so the dashboard's "first-seen 40
  minutes ago" / "attempt #41" badges persist across the natural
  FUSE retry-via-recall pattern. Cleared on terminal exits (success,
  fatal, stale-path prune).

## [0.13.0-beta.5] - 2026-05-07

### Changed
- **Dashboard in-flight view now distinguishes a healthy upload from a
  retry loop.** The previous view showed only path/size/elapsed —
  identical for "fresh 12s upload" and "retry #5 of a 40-minute
  failure spiral." Each in-flight row now also shows attempt count
  (when > 1), first-seen elapsed, and live byte progress (`5.2
  MB/100 MB 5%`) parsed from `rclone --stats=1s` output. The
  current-attempt elapsed remains the primary clock; the rest are
  conditional add-ons that only render when meaningful.

### Added
- `Backend::upload_with_progress` (default-implemented to fall back to
  plain `upload`). `RcloneBackend` overrides with a streaming
  `run_with_progress` that pipes rclone's stderr line-by-line and
  forwards `Transferred: X / Y` quantities through a `Sender<u64>`.
- `ipc::ActiveUpload` gains `first_started_unix`, `attempt`, and
  `bytes_uploaded` fields. All three default-deserialize for legacy
  daemons during partial-upgrade rollouts.

### Notes
- Cycle observed by users where the timer "fluctuates between 0 and
  60 seconds" was a transient-error retry loop. The 60s cadence comes
  from `mount.poll_interval` being reused as the upload-retry
  debounce in `main.rs:209` — that's a separate quirk and not
  changed here. The dashboard now makes the loop visible so it's
  diagnosable without reading journal logs.

## [0.13.0-beta.4] - 2026-05-07

### Fixed
- **OneDrive delta: skip items with unrecognized `parentReference.path`
  shapes** instead of passing the raw string through as the parent
  path. The pass-through fallback could land items at the mount root
  or store malformed `drives/{id}/items/...` strings as `remote_path`,
  the same class of bug as the SharePoint duplicate-folder issue
  (v0.12.x). Live items dropped on the delta channel are picked up on
  the next rclone poll cycle; deletes are caught by the id-aware
  dispatcher (milestone 3) or the verifying-stat self-heal. The warn!
  line gained `id`, `name`, `deleted`, `is_folder`, `is_file` fields
  so unexpected shapes can be diagnosed from journal logs without
  inferring identity from the raw path alone. Soak target: this warn
  should be very rare; rising rates point at a real shape Graph is
  emitting that we should add explicit support for.

## [0.13.0-beta.3] - 2026-05-07

### Added
- **`stratosync cache clear` subcommand** — operator escape hatch for
  when local state has drifted out of sync with the remote. Drops
  hydrated cache files and reverts their `file_index` rows from
  `cached`/`stale` back to `remote`, so the next `open()` re-hydrates
  fresh from the cloud.
  - `--mount NAME` to clear a single mount; `--all` to do every
    enabled mount.
  - **Unsynced work is preserved**: rows in `dirty`, `uploading`, or
    `conflict` status are never touched. Pinned files are also
    preserved unless `--include-pinned` is passed.
  - Refuses to run while the daemon is up (probes the IPC socket);
    pass `--force` to override at your own risk. Interactive
    confirmation by default; `-y/--yes` for scripts.
  - New core helper `StateDb::clear_cache_for_mount(...)` returns a
    `CacheClearReport` with files cleared, bytes freed, and the cache
    paths the caller must unlink.

## [0.13.0-beta.2] - 2026-05-06

### Added
- **v0.13 milestone 3: ID-aware delete events**. `RemoteChange::Deleted`
  now carries `item_id: Option<String>` alongside `path`. OneDrive and
  Google Drive populate it from their respective stable IDs. The
  poller's delta-mode delete handler tries
  `delete_remote_entry_by_item_id` first and falls back to the
  path-based variant. Closes the renamed-then-deleted-in-the-same-
  delta-page case where the delete event's path could be stale —
  ID-based matching still finds the right row.
- **Self-heal log enrichment**: when `do_hydrate` prunes a stale row,
  the warn! line now distinguishes legacy NULL-id rows ("expected,
  tapers as IDs backfill") from rows that already had a known
  item_id ("id-aware upsert may have missed a delta event"). Soak
  signal: known-id pruning should be very rare; if it isn't, the
  upsert path needs investigation.

### Notes
- Backends without stable IDs (WebDAV, raw S3) keep delivering
  `Deleted { item_id: None }`; the path fallback handles them as
  before — no behavior change.
- The v0.12.3 self-heal stays in place as defense-in-depth.
  Removal awaits soak data showing the new-warning rate is near
  zero across real workloads.

## [0.13.0-beta.1] - 2026-05-06

### Changed
- **MSRV bumped to Rust 1.85** (was 1.80). All exact-version dep pins
  removed: `clap`, `toml`, `toml_edit`, `fuser`. `fuser` 0.15 changed
  the `getattr` callback signature — adapted (`Option<u64>` fh added).
  Update with `rustup update stable` if you're below 1.85.

### Added
- **v0.13 milestone 2: lazy item_id backfill in the self-heal**. When
  `do_hydrate`'s download fails NotFound and the verifying `stat()`
  succeeds, the stat result's `item_id` (if any) is written onto the
  DB row via the new `set_item_id_if_absent` API. The IS NULL guard
  ensures we never overwrite an authoritative ID with a possibly-
  stale stat value. Effect: pre-v0.13 rows graduate into the id-aware
  upsert path the first time they trigger the band-aid, so the *next*
  rename event for the same item gets handled in place by milestone 1
  instead of looping through the band-aid forever.

### Notes
- The v0.12.3 stat-on-NotFound self-heal stays in place as defense-
  in-depth for backends without stable IDs (WebDAV, raw S3) and for
  not-yet-backfilled rows. It also now does the lazy backfill described
  above. Removal of the band-aid awaits a future release once the
  installed base is fully migrated.
- Legacy `upsert_remote_file` / `upsert_remote_file_gen` are now thin
  wrappers around `upsert_remote_file_by_id_or_path` (one source of
  truth). Their public signatures are unchanged for back-compat with
  existing test fixtures and any downstream caller.

## [0.12.4] - 2026-05-06

### Added
- **v0.13 milestone 1: ID-aware rename detection**. The DB now stores
  the backend's stable item ID (Microsoft Graph item ID, Google Drive
  file ID) alongside each row's `remote_path`. A new
  `StateDb::upsert_remote_file_by_id_or_path` matches on the ID first,
  so a OneDrive `Modified`-with-new-path event for a renamed file
  updates the existing row in place instead of leaving a stale row at
  the old path. The full-poll path uses the same upsert so item IDs get
  populated even for backends that aren't running on the delta channel
  yet. Schema migration `0008_remote_item_id.sql` adds a nullable
  `remote_item_id` column with a partial unique index. Backends that
  don't expose stable IDs (WebDAV, plain S3) keep the legacy path-based
  behavior — fall-through is automatic.

  This is the proper fix for the bug v0.12.3 self-healed via stat-on-
  NotFound. The self-heal stays in place as defense-in-depth for any
  pre-v0.13 row whose ID hasn't been backfilled yet, and for backends
  without a stable-ID concept. Backfill of existing rows + removal of
  the band-aid lands in milestone 2.

## [0.12.3] - 2026-05-06

### Fixed
- **Large-file copy from a renamed-on-remote folder loops EAGAIN
  forever**: when a file's parent directory was renamed on the remote
  via OneDrive (and other delta-style providers), our DB row kept its
  old `remote_path` because the delta channel emits Modified-with-new-
  path for renames *without* a paired Deleted-old-path event. The
  user's `cp` then saw the FUSE `read()` retry indefinitely with
  EAGAIN as `do_hydrate` kept calling rclone with a stale path that
  returned "directory not found." `do_hydrate` now does a one-shot
  `backend.stat()` whenever a download fails NotFound; if stat
  confirms the path is gone, the row is pruned and the FUSE read
  fails ENOENT cleanly. The kernel's next directory lookup
  repopulates from a fresh listing under the file's new path. Stat-
  succeeds (transient backend glitch) keeps the row intact and the
  retry path unchanged. Error messages now include the offending
  `remote_path` so future occurrences are diagnosable from a journal
  glance.

## [0.12.2] - 2026-05-06

### Added
- **Hydration health surfaced in tray, dashboard, and Prometheus**: when
  downloads (FUSE `open()` hydrations) start failing, a new
  `mount_health` table records consecutive failures and the last error
  message. The system tray flips to a `dialog-warning` icon (and names
  the stalled mount in its tooltip) at 3 consecutive failures so users
  notice the problem without grepping the journal. The dashboard TUI
  shows the same data in the poller pane (`hydration: N consecutive
  fail(s) — …`) and folds it into the per-mount status column. A new
  `stratosync_mount_hydration_consecutive_failures` Prometheus gauge is
  emitted for scrape-based alerting. Successful hydrations reset the
  counter but keep `last_hydration_error` so post-recovery diagnostics
  still work. Schema migration `0007_mount_health.sql`.

  This is a visibility fix only — the underlying "ino=… not found:
  directory not found" error during large copies is tracked separately
  (likely a delta-poller rename racing an in-flight download).

## [0.12.1] - 2026-04-30

### Fixed
- **Dolphin: "not enough room on the device" when pasting** — `StratoFs`
  now implements the FUSE `statfs` callback, returning host-filesystem
  totals from `statvfs(2)` on the cache directory. Without this the
  fuser default reported zero blocks/inodes and KIO's copy-job
  pre-flight refused. Nautilus and `cp` skipped the pre-flight, which
  is why the symptom was Dolphin-only.
- **Copy-overwrite via Dolphin/Nautilus failed with "cache file
  missing: …/.goutputstream-XXX"** — race between `handle_rename` and a
  concurrent in-flight upload. `run_upload` snapshots `cache_path` at
  start; if a `g_file_move` rename arrived mid-upload, the cache file
  moved on disk and the DB row's `cache_path` was updated, but the
  upload finalizer used `set_cached`, which clobbered the rename's new
  path back to the stale temp-file path. The next debounced upload
  trigger then read a phantom path. Added `StateDb::set_uploaded`
  (status/etag/size/mtime only — leaves `cache_path` alone) and
  switched the upload finalizer to it. CLI `cp` was unaffected because
  it doesn't fsync, so there was no overlap between upload and rename.
- **Dolphin: new folders/files not appearing until you re-enter the
  parent** — directory mutations (`mkdir`/`create`/`unlink`/`rmdir`/
  `rename`) now invalidate the kernel's readdir cache for the affected
  parent inode(s). `mount()` was refactored to use `fuser::Session`
  directly so a `Notifier` can be plumbed into `StratoFs`; enabling
  `fuser`'s `abi-7-12` feature exposes `inval_inode`. Nautilus picked
  up the changes via inotify regardless; Dolphin/KIO needed the
  explicit kernel-cache invalidation.
- **OneDrive duplicate folders at the root of the mount**: the OneDrive
  delta provider's `resolve_item_path` only stripped the `/drive/root:`
  form of `parentReference.path`. Microsoft Graph also returns
  `/drives/{drive_id}/root:/...` for non-default drives (SharePoint,
  business OneDrive with a `drive_id` configured). Items from the
  second form fell through to a raw passthrough and got persisted as
  e.g. `drives/4139BC84D0721EB5/root:/test`. The rclone poller
  meanwhile reported the same item with the clean `test`, so the
  `(mount_id, remote_path)` UNIQUE didn't catch the collision and the
  DB ended up with two rows per affected entry. Pre-v0.12.1 the
  kernel readdir cache happened to hide the duplicates; v0.12.1's
  post-mutation `inval_inode` exposed them. Two changes:
   - `resolve_item_path` now strips both forms via a shared
     `strip_onedrive_root_prefix` helper.
   - Migration `0006_dedupe_file_index.sql` collapses any existing
     duplicates (lowest inode wins; child rows reparented onto it) and
     adds a partial `UNIQUE(mount_id, parent_inode, name)` index so
     future writes can't reintroduce the bug. Runs automatically on
     daemon startup.
- **Defensive readdir dedup** (belt-and-suspenders): `fuse::readdir`
  also hides any in-memory rows that share `(parent_inode, name)` with
  a sibling — lowest inode wins, matching what `lookup` returns. With
  migration 0006 in place this should never fire in practice; if it
  does, the warning log surfaces it for follow-up.
- **30-second-plus context-menu hangs in Dolphin on folders of large
  media files**: KIO opens every selected file and reads 16–64 KiB
  from offset 0 to sniff MIME / generate thumbnails on right-click.
  Each read on a `Remote` file used to be a fresh `rclone cat --offset
  0 --count 32768` invocation (process spawn + OAuth refresh + cloud
  round-trip ≈ 1–3 s), and KIO's sniff loop is *serial* — read, await
  reply, read next — so right-clicking N selected files blocked the
  UI for ~3 s × N. Worse, our `open()` was *also* optimistically
  kicking off a full `rclone copyto` for every file in the background,
  so a casual right-click on five MP4s would queue up multi-GB
  downloads. Five interacting changes that together drop right-click
  on a primed folder of MP4s from ~45 s to instant:
   - **Header cache** (`<cache_dir>/.meta/headers/<inode>`): persists
     the first N bytes of a file at owner-only perms, atomic
     temp-rename writes. `read()` serves any in-range sniff from the
     local file. New config knob `[sync] header_prefetch_size`
     (default `"64 KB"`, `"0"` to disable). Headers are dropped on
     rename-overwrite, unlink, and any poller-detected remote change.
   - **Background `spawn_prefetch_headers`** runs on every `readdir`
     for files over `[sync] prefetch_threshold` (the small-file pass
     already hydrates everything below it in full). Idempotent —
     skips files that already have a header on disk OR are tracked in
     a daemon-wide `prefetch_inflight` DashMap, so concurrent reads
     and concurrent readdirs of the same dir never duplicate work.
   - **Focus-aware queue**: each `readdir` records its target inode
     in `prefetch_focus_dir`. Queued prefetch tasks check it after
     waking from the semaphore and bail if the user has navigated
     elsewhere — so the user's *current* directory isn't stuck
     behind 600 stale headers from sidebar-tree readdirs. (Without
     this, KIO's tree-view + breadcrumb panel walking 30 dirs sent
     the user's actual right-click target to the back of a 3-min
     queue.) Daemon-wide concurrency cap is 16 simultaneous
     rclone-cat invocations.
   - **Async-spawned `read()` handler**: was `rt.block_on`, now
     `rt.spawn` so the FUSE worker thread frees up immediately to
     accept the next request. Replies fire from inside the spawned
     task once work completes (`ReplyData` is `Send`). Doesn't help
     when the *caller* serializes (KIO does), but unlocks parallelism
     for callers that don't.
   - **Deferred auto-hydrate for huge files**: files larger than
     `[sync] auto_hydrate_max_size` (default `"100 MB"`) are no
     longer full-downloaded by `open()`. They still hydrate on the
     first read past the header — that's the user actually accessing
     content rather than KIO sniffing — so streaming and `cat`-style
     use still work. Set to `"0"` to keep legacy v0.12.0 behavior.
  Verified live against a folder of eleven 1–2 GB Google-Drive MP4s
  through Dolphin: from ~45 s freeze pre-fix → instant post-fix once
  the focus pass completes (~3 s after navigating into the folder).
- **`upload fatal: dirty but no cache_path` warning storm on every
  daemon start** — `get_pending_uploads()` returned every row with
  `status IN ('dirty','uploading')` regardless of kind, so directory
  rows that had been left in a dirty state (legacy alpha data or some
  setattr edge case) were re-queued for upload on every restart, then
  crashed `run_upload` (directories have no cache_path) and spammed
  desktop notifications. Three layers:
   - SQL filter: `get_pending_uploads()` now restricts to `kind='file'`.
   - One-shot startup cleanup: `reset_stuck_dirty_directories()`
     resets any `kind='dir' AND status IN ('dirty','uploading')`
     rows to `'cached'` so the queue stops re-discovering them
     across restarts.
   - Defense in depth in `run_upload`: a non-file inode that slips
     through (e.g. via a stray `Write` trigger) is silently reset to
     `Cached` and skipped, no fatal-notification.

## [0.12.0-beta.1] - 2026-04-29

### Added
- **Dolphin emblem-overlay plugin (KF6)**: new `KOverlayIconPlugin` C++
  plugin under `contrib/file-managers/dolphin/overlay-plugin/`. Reads
  `user.stratosync.status` via `getxattr(2)` and returns the same
  freedesktop emblem names the GTK plugins use (cached/dirty/uploading/
  hydrating/remote/conflict/stale → emblem-default/synchronizing/
  downloads/web/important/generic). Ships as a separate
  `stratosync-dolphin-overlay` subpackage in `.deb` and `.rpm` so non-KDE
  installs don't pull KF6 KIO at runtime; AUR PKGBUILD adds `kio` as an
  optdepend on the main package. install.sh detects KF6 KIO devel
  headers and builds the plugin opt-in into `~/.local/lib/qt6/plugins/`.

- **Phase 6 declarative slice**: context-menu actions now ship for
  Dolphin / Konqueror (KDE), Thunar (XFCE), and PCManFM / PCManFM-Qt
  (LXDE/LXQt). New shared shell wrapper
  `contrib/file-managers/bin/stratosync-fm-action` checks
  `user.stratosync.status` is present, dispatches to the right
  `stratosync` subcommand, surfaces errors via `notify-send`, and
  detaches via `setsid` so the FM never blocks. Per-FM glue:
  - **Dolphin / Konqueror** — KIO ServiceMenu `.desktop` under a
    *Stratosync* submenu. Goes to `usr/share/kio/servicemenus/`.
  - **PCManFM / PCManFM-Qt** — FreeDesktop file-manager Actions spec,
    one `.desktop` per item under `usr/share/file-manager/actions/`.
  - **Thunar** — UCA (Custom Actions) snippet under
    `share/doc/stratosync/thunar/` for manual merge into the user's
    `~/.config/Thunar/uca.xml` (auto-merge would clobber user
    actions). README documents the merge.

  These ship context-menu items only — Dolphin emblem overlays need a
  C++ `KOverlayIconPlugin` and Thunar/PCManFM have no emblem API at
  all (tracked as ROADMAP follow-ups).

- **Phase 6 GTK slice**: file-manager integration extended beyond Nautilus.
  New `contrib/file-managers/common/stratosync_fm_common.py` shared helper
  (xattr reading, status→emblem mapping, CLI shell-out, single
  `menu_items_for(paths)` generator with full unit-test coverage). The
  Nautilus extension moved into `contrib/file-managers/nautilus/`, gained
  a `MenuProvider` with Pin / Unpin and three conflict-resolution items
  (keep-local / keep-remote / 3-way merge), and now uses the shared
  helper. New `nemo` (Cinnamon) and `caja` (MATE) extensions are thin
  wrappers over the same helper, with parity emblems and menu items.
  install.sh / `.deb` / `.rpm` / AUR PKGBUILD all install the helper +
  whichever extensions match installed Python bindings; python3-nautilus
  is a Recommends, python3-nemo / python3-caja are Suggests.
- **Dashboard TUI** (`stratosync dashboard`): live ratatui-based view of
  per-mount sync state, hydration queue, upload queue, and poller status.
  Backed by a new daemon IPC socket that aggregates per-mount status from
  the existing state DB and queue handles.
- **Conflicts cleanup CLI** (`stratosync conflicts cleanup`): walks the
  conflict namespace, drops siblings whose content is byte-equal to the
  canonical file (the common case after a transient false-positive),
  shows per-entry progress, and accepts `--dry-run`. Includes a
  stat-based fast path and live progress output for large trees.
- **File versioning (Phase 5, item 5)**: per-mount
  `version_retention` (default 10) records snapshots of file content
  at the moments it would otherwise be lost — local cache content
  before a poller-detected remote change replaces it (`before_poll`),
  and the just-uploaded content after a successful upload
  (`after_upload`). Snapshots reuse the existing content-addressed
  `BaseStore`, so identical content across versions deduplicates
  automatically. New `stratosync versions list <path>` and
  `stratosync versions restore <path> --index N` CLI commands let
  users recover prior versions; restore copies the blob back to the
  cache and marks the file Dirty for re-upload. Files larger than
  `[daemon.sync] base_max_file_size` are skipped to bound disk
  usage. Set `version_retention = 0` to disable.
- **Multiple accounts per provider (Phase 5, item 4)**: documented as
  already-supported. Each `[[mount]]` block is fully isolated — its
  own SQLite DB, cache directory, poller, upload queue, FUSE thread,
  and (with `webdav_sidecar = true`) WebDAV sidecar on a distinct
  port. Configuring two Google Drive accounts is a matter of two
  rclone remotes plus two mount blocks. No code change.
- **Bandwidth scheduling (Phase 5, item 3)**: per-mount
  `upload_window = "HH:MM-HH:MM"` (local time, wraparound supported).
  Outside the window the upload queue holds dirty files but does not
  dispatch; the loop sleeps until the window reopens, no busy-wait. In-
  flight uploads are not interrupted. `fsync()` always bypasses the
  schedule — the user explicitly asked for durability, the bandwidth
  policy is coarse and shouldn't override that. Polling is unaffected;
  this gates uploads only.
- **Prometheus metrics endpoint (Phase 5, item 2)**: optional
  `GET /metrics` HTTP endpoint exposing per-mount cache usage, queue
  depth, hydration counts, conflict count, and poller state. Opt-in via
  `[daemon.metrics] enabled = true` (default off, default bind
  `127.0.0.1:9090`). Hand-rolled HTTP/1.1 listener on `tokio::net`
  rather than pulling in axum/hyper-server for a single endpoint —
  reuses the existing `DaemonState::snapshot()` aggregation.
- **Selective sync (Phase 5, item 1)**: per-mount `ignore_patterns`
  config field filters glob-matched paths out of indexing, FUSE
  `create`/`mkdir`, and upload events. Patterns use `globset` semantics
  (`*` matches across `/`, so `*.log` catches log files at any depth;
  use `node_modules/**` for subtrees). Bad patterns fail the daemon at
  startup with a clear error. Already-indexed entries that match a
  newly-added pattern are preserved — rules prevent new indexing, never
  retroactively unindex. Ignored FUSE creates return `EPERM` so apps see
  a visible failure rather than silently writing into an untracked file.

### Changed
- **Conflict files isolated under `.stratosync-conflicts/`**: conflict
  siblings are stored on the remote under a dedicated prefix instead of
  alongside their canonical files, keeping the user-visible namespace
  clean. Poller, watcher, and upload queue filter the conflict prefix
  on every ingress so files don't get re-imported or re-uploaded as
  regular content. Added a startup tree walk to migrate any
  pre-existing conflict files into the new namespace.
- **Poller runs immediately on startup** instead of sleeping the first
  poll interval, so directory listings populate without latency on
  first navigation. Directories are also bulk-marked as `dir_listed`
  after a full recursive poll, eliminating per-directory backend
  `list()` calls during traversal.

### Changed
- **`on_file_cached` no longer triggers eviction inline.** Hydrations
  used to call `maybe_evict()` synchronously to be aggressive about
  quota enforcement. In practice that turned every successful
  hydration into a multi-second StateDb-mutex storm whenever the
  eviction loop was misbehaving — observed live at ~17 s tail latency
  on every `ls` while the mount was over quota. Eviction is now driven
  exclusively by the 60 s ticker; worst-case quota lag is one tick,
  acceptable given the 90 % high-water mark headroom. Hydrations stay
  fast and the eviction loop's blast radius is bounded. New regression
  test (`on_file_cached_does_not_trigger_eviction`) seeds a mount that
  is 10× over quota and asserts that `on_file_cached` only updates the
  LRU position without running an eviction pass.

### Fixed
- **OneDrive delta polling halted permanently after one nameless item.**
  Microsoft Graph occasionally returns `driveItem`s in a delta response
  without a `name` field — observed live on a real account, where one
  page contained a stub with `id` + `createdDateTime` +
  `parentReference` and no name. The receiver had `name: String`
  (required) on `OneDriveItem`, so serde failed the *entire page*.
  Every poll then errored, the OneDrive mount eventually halted after
  consecutive failures, and the user was left without OneDrive sync.
  Now `name: Option<String>`; nameless items are skipped at the
  per-item loop (path resolution needs the name anyway). Also improved
  the parse-error path to capture and log a 512-char body snippet —
  without that, the upstream `error decoding response body` was opaque
  and required code patches to diagnose. Live-verified: zero
  parse-error WARNs over 2+ poll cycles where pre-fix every poll
  failed.
- **Stale partial hydrations leaked disk space indefinitely.** Files
  under `<cache_dir>/.meta/partial/` are temp blobs that `do_hydrate`
  creates, fills via `backend.download()`, then renames onto their
  final `cache_path` on the success path. Any interruption — daemon
  crash, `kill -9`, rename failure, retry from scratch — left the
  temp file behind, and there was no startup wipe. One observed mount
  had **6 partial files totalling ~14 GiB on disk** with no DB row to
  reach them; eviction couldn't see them at all. New startup
  reconciler in `cache::reconcile` walks the per-mount cache dir and
  (a) removes regular files whose path isn't in `file_index.cache_path`,
  (b) unconditionally wipes everything in `.meta/partial/` (orphan by
  construction). Skips `.bases/` (BaseStore-managed) and the rest of
  `.meta/` (daemon scratch). Runs once per mount before the FUSE
  mount comes up so it can't race with active reads. 4 unit tests
  cover the orphan-walker, the partial-wipe, the empty-cache no-op,
  and the missing-dir guard. Live-tested: a daemon at 22 GiB disk /
  8 GiB DB-tracked dropped to 8 GiB disk after a single restart.
- **Cache eviction stalled forever on phantom DB rows.** When a `cached`
  row pointed at a `cache_path` that no longer existed on disk (manual
  `rm`, prior crash mid-eviction, any out-of-band cleanup),
  `tokio::fs::remove_file` returned `ENOENT` and the eviction loop logged
  a WARN, skipped the row, and never reconciled the DB. The phantom row
  stayed counted in `total_cache_bytes`, was returned again on the next
  pass, ENOENT'd again. With many phantoms in the candidate set,
  `pass_freed == 0` and the loop bailed — eviction did nothing for the
  rest of the daemon's life. One observed daemon had 1000 phantom rows
  hit the bail-out on the first pass after restart. Now ENOENT is treated
  as success: the disk space is already freed, so we call `set_evicted`
  to clear the DB row and add `cache_size` to the pass's progress.
  Includes a regression test (`maybe_evict_reconciles_phantom_rows`)
  that seeds 100 phantom rows and confirms eviction converges.
- **Cache eviction never converged when far over quota.** `maybe_evict()`
  queried `lru_eviction_candidates(mount_id, 200)` once per pass and stopped
  after walking those 200 rows. With many small files (typical screenshots,
  text docs) freeing 200 × ~191 KB ≈ 38 MB per pass — combined with the
  60 s loop interval — was ~2.3 GB/hour theoretical max, easily out-paced
  by hydration. The cache stayed permanently over quota, sometimes by a
  large multiple (one live mount observed at 167 % full, 18 GB on a
  10.7 GB quota). The eviction code now paginates: re-queries the next
  1000-candidate batch each iteration (cheap because `set_evicted()` removes
  rows from `cache_lru`), bails on no-progress to avoid spinning, and keeps
  going until usage is below the low-water mark. Added two regression tests
  covering the convergence path on a many-small-files mount and the
  no-op-when-under-high-mark fast path.
- Conflict sibling lookup used the wrong column name (`parent` vs
  `parent_inode`), causing false negatives in the conflicts cleanup
  walker.
- Deadlock and orphan handling in conflicts cleanup when a sibling's
  parent had already been removed.
- `WebDavSidecar` carried an unused `port` field; removed.

## [0.11.0] - 2026-04-11

### Added
- **WebDAV sidecar backend**: Optional `rclone serve webdav` subprocess per mount
  for low-latency HTTP-based transfers instead of spawning rclone per operation.
  Enable with `[daemon] webdav_sidecar = true`. Implements full Backend trait
  via HTTP/WebDAV protocol (GET, PUT, PROPFIND, MKCOL, DELETE, MOVE).
- **Nautilus file manager extension**: Python GObject extension reads
  `user.stratosync.status` xattr to show sync status emblem overlays
  (checkmark for cached, sync arrows for uploading, warning for conflicts).
  Supports Nautilus 3.0 and 4.0.
- **System tray indicator** (`stratosync-tray`): New workspace crate using
  `ksni` (StatusNotifierItem). Polls mount databases every 5s, shows per-mount
  cache usage, syncing count, conflicts, and pinned files. Autostart desktop
  file included.
- **Distribution packaging templates**: Debian `.deb` (via debian/), Fedora
  `.rpm` (spec file), and Arch AUR (PKGBUILD). All include binaries, systemd
  unit, and Nautilus extension.
- `RcloneBackend::which_rclone()` public method for locating the rclone binary.

### Changed
- **Phase 4 complete**: all planned Phase 4 deliverables are now implemented.

## [0.10.0] - 2026-04-11

### Added
- **Pin/unpin for offline availability**: `stratosync pin <path>` downloads and pins
  files so they survive cache eviction. Supports recursive directory pinning.
  `stratosync unpin <path>` releases the pin. `stratosync status` shows pinned count.
- **Background hydration with range-read fast path**: `read()` on a file that's still
  downloading now uses `rclone cat --offset/--count` to serve the requested bytes
  immediately while the full download continues in the background. Falls back to
  blocking wait for backends that don't support range requests.
- **`download_range()` Backend trait method**: New `download_range(remote, offset, len)`
  with default `NotSupported` fallback. Implemented for RcloneBackend and MockBackend.
- **`SyncError::NotSupported` variant** for graceful feature detection.
- **Readdir small-file prefetch**: When a directory is listed for the first time,
  files under `prefetch_threshold` (default 1 MB) are hydrated in the background.
  Configurable via `[daemon.sync] prefetch_threshold = "1 MB"` (set to "0" to disable).
- **StateDb pinning methods**: `set_pinned()`, `is_pinned()`, `pinned_count()`,
  `list_file_descendants()`.

## [0.9.0] - 2026-04-11

### Added
- **`conflicts resolve` CLI**: four subcommands for resolving conflict files:
  - `stratosync conflicts keep-local <path>` — upload local version, discard remote conflict
  - `stratosync conflicts keep-remote <path>` — download remote version, discard local
  - `stratosync conflicts merge <path>` — attempt 3-way merge using base version store
  - `stratosync conflicts diff <path>` — show unified diff between local and remote
- **Desktop notifications for upload failures**: `notify-send` alerts when uploads
  fail fatally, in addition to existing conflict notifications.
- **xattr sync status**: read-only extended attributes on every FUSE-mounted file:
  - `user.stratosync.status` — current sync state (remote, cached, dirty, uploading, conflict)
  - `user.stratosync.etag` — remote version identifier
  - `user.stratosync.remote_path` — path on the remote backend
  - `setxattr`/`removexattr` return `ENOTSUP` (read-only).
- **`stratosync-core::merge` module**: extracted `MergeOutcome`, `try_three_way_merge()`,
  and `git_available()` from the daemon into core for CLI reuse.
- **Shared notification module** (`sync::notification`): extracted from conflict.rs
  for reuse across upload queue and conflict resolver.
- AWS S3 setup script (`scripts/setup-aws-s3.sh`) for bucket, IAM, rclone,
  and stratosync config in one step.

### Changed
- **Phase 3 complete**: all planned Phase 3 deliverables are now implemented.

## [0.8.0] - 2026-04-10

### Added
- **3-way text merge for conflict resolution**: When `text_conflict_strategy = "merge"`
  is set, concurrent edits to text files are resolved automatically via
  `git merge-file`. Non-overlapping changes merge cleanly with no conflict
  file; overlapping changes produce conflict markers. Falls back to keep-both
  for binary files, when git is unavailable, or when no base version exists.
- **Base version object store** (`BaseStore`): Content-addressed SHA-256 store
  at `{cache_dir}/.bases/objects/`. Base versions are captured after each
  successful upload and used as the common ancestor for 3-way merge. Includes
  automatic eviction of stale bases (configurable retention, default 30 days).
- **`SyncConfig` merge settings**: `text_conflict_strategy` (keep_both/merge),
  `base_retention_days`, `base_max_file_size`, `text_extensions` allowlist.
- **`base_versions` DB table** with reference counting for deduplication.
- 6 end-to-end conflict resolution tests (clean merge, conflict markers,
  no-base fallback, binary skip, strategy toggle, no-git fallback).
- **Desktop notifications** on conflict via `notify-send`.

### Fixed
- **ETag conflict detection used file IDs, not content hashes**: `stat()` was
  called without `--hash`, so rclone returned no content hashes. Google Drive
  file IDs don't change on content update, making conflicts undetectable.
  Now `stat()` always requests hashes.
- **Hash key case mismatch**: rclone outputs lowercase hash names (`sha1`,
  `md5`) but the code looked for uppercase (`SHA-1`, `MD5`). Now checks both.
- **Upload queue race condition**: The abort-and-respawn debounce mechanism
  had multiple race conditions causing uploads to be silently dropped.
  Redesigned to deadline-based debounce (no oneshot channels, no abort
  mechanism).
- **Watcher inode mismatch after poller upsert**: The inotify watcher looked
  up inodes via `cache_path` column, which is NULL for entries replaced by
  the poller. Now derives `remote_path` from the cache file path and uses
  `get_by_remote_path()`.
- **Delta poller proceeds when initial listing fails**: Instead of blocking
  delta mode, logs a warning and enters delta-only mode.

### Changed
- **Upload queue**: Complete rewrite of debounce mechanism from abort-and-
  respawn (oneshot channels + JoinSet) to deadline-based (HashMap of
  `inode -> due_at`). Eliminates all race conditions.
- **`RcloneBackend::stat()`**: Now includes `--hash` flag for content-based
  change detection.

## [0.7.2] - 2026-04-10

### Fixed
- **Google Drive 403 rate limits**: Google returns quota errors as HTTP 403
  with `rateLimitExceeded` in the body, not 429. The error mapper now checks
  the body for rate-limit keywords before falling through to PermissionDenied,
  so the poller correctly backs off instead of halting.
- **OAuth token refresh via rclone**: Token refresh now always delegates to
  rclone (`rclone about` to trigger refresh, then `rclone config show` to
  read the fresh token). The previous direct-API refresh failed when rclone
  used its built-in shared credentials (client_id not in config).
- **Stale token after expiry**: `rclone config show` only reads the config
  file — it does not trigger an OAuth refresh. Added a two-step process:
  `rclone about <remote>:` forces authentication (refreshing the token),
  then `rclone config show` reads the now-fresh token.
- **Redundant full listings on rate limit**: When the initial `get_start_token`
  failed after a successful full listing, the token was never stored, causing
  every retry to redo the full listing. Now the poller detects existing DB
  entries and skips the listing, only retrying the token acquisition.
- **401 retry with force-refresh**: `changes_since` and `start_token` now
  catch HTTP 401, force-refresh the OAuth token via rclone, and retry once
  before failing. Handles the race where the token expires between the
  expiry check and the actual API call.

## [0.7.1] - 2026-04-10

### Added
- **OneDrive delta polling**: The remote poller now supports OneDrive's delta
  API (`deltaLink`) for incremental change detection, using Microsoft Graph.
  Enabled automatically when the rclone remote type is `onedrive`. OneDrive's
  delta API returns full paths (via `parentReference.path`), making path
  resolution simpler than Google Drive's ID-based approach.
- **`OneDriveDelta` implementation**: Full `DeltaProvider` implementation with
  OAuth token refresh via Microsoft's token endpoint, pagination support,
  `token=latest` for start tokens, and `resyncRequired` error handling.
- 17 new OneDrive-specific unit tests (JSON parsing, path resolution, error
  mapping).

### Changed
- **`RcloneBackend::init_delta`**: Now constructs `OneDriveDelta` for
  `type = onedrive` remotes (previously logged a warning and fell back to
  full listing). Supports `drive_id` config for business/shared drives.

## [0.7.0] - 2026-04-10

### Added
- **Google Drive delta polling**: The remote poller can now use Google Drive's
  Changes API (`pageToken`) for incremental change detection instead of full
  recursive listings every poll cycle. This dramatically reduces API calls and
  latency for large mounts. Enabled automatically when the rclone remote type
  is `drive`.
- **`DeltaProvider` trait**: New extensible trait in `stratosync-core` for
  provider-specific delta APIs. `GoogleDriveDelta` is fully implemented;
  `OneDriveDelta` is stubbed for future implementation.
- **`SyncError::TokenExpired`** variant for change token invalidation (HTTP 410).
  The poller automatically falls back to a full listing and obtains a fresh
  token when this occurs.
- **rclone config parser**: Reads OAuth credentials from `rclone config show`
  output, handling encrypted configs and rclone's built-in credentials.
- **`StateDb` change token methods**: `get/set/clear_change_token()` for opaque
  delta token storage, and `delete_remote_entry_by_path()` for individual
  entry deletion (used by delta polling).
- 10 new delta integration tests, 18 unit tests for the delta module, 11 for
  the rclone config parser, 7 for the StateDb additions.
- **OneDrive delta stub**: Provider detection for `onedrive:` remotes is in
  place; implementation will follow in a future release.

### Changed
- **`Backend` trait**: Added `get_start_token()` method (with default impl)
  for obtaining initial change tokens.
- **`RcloneBackend`**: Now holds an optional `DeltaProvider` initialized at
  startup via `init_delta()`. `supports_delta()` and `changes_since()` delegate
  to the provider when available.
- **`MockBackend`**: Extended with `enable_delta()`, `push_change()`, and
  `set_delta_error()` for testing delta workflows.

### Known Limitations
- Google Drive shared drives are not supported (requires `driveId` parameter).
- Path resolution for deeply nested files may require additional API calls for
  parent chain resolution (mitigated by in-memory cache per poll cycle).

## [0.6.1] - 2026-04-09

### Fixed
- **mkdir + immediate file create**: Creating a file inside a newly-made
  directory failed with EIO because `populate_directory` tried to list the
  directory on the remote before the async `mkdir` had completed. Now, if
  the backend listing fails for a locally-created directory (status is not
  `Remote`/`Stale`), the directory is treated as empty and marked listed so
  that `create`/`lookup` inside it can proceed immediately.

## [0.6.0] - 2026-04-09

### Fixed
- **Multi-mount isolation bug**: Two mounts with different rclone remotes showed
  identical content. Root cause: all mounts shared a single `state.db`, causing
  root inode collisions (`INSERT OR REPLACE` at inode 1) and unfiltered
  `list_children`/`get_by_parent_name` queries that returned cross-mount entries.

### Changed
- **Per-mount database files**: Each mount now gets its own SQLite database
  (`{name}.db`) instead of sharing `state.db`. This provides complete isolation
  of inode namespaces and file entries between mounts.
- **`StateDb::list_children` and `get_by_parent_name`** now require a `mount_id`
  parameter for defense-in-depth filtering (breaking API change for downstream
  consumers of `stratosync-core`).
- CLI commands (`status`, `conflicts`) open per-mount database files.

### Migration
- The old shared `state.db` is no longer used. The daemon will create new
  per-mount databases on first run. Cached files from the old DB will need to
  be re-hydrated.

## [0.5.2] - 2026-04-09

### Fixed
- **Systemd service: remove PrivateTmp and NoNewPrivileges** — `PrivateTmp=true`
  creates a private mount namespace that makes the FUSE mount invisible outside
  the daemon's process. `NoNewPrivileges=true` blocks the setuid `fusermount3`
  binary from elevating privileges, causing "Operation not permitted" on mount.
  Both directives have been removed from the service template with explanatory
  comments documenting why they are incompatible with FUSE.

## [0.5.1] - 2026-04-07

### Fixed
- **Hydration timeout**: `read()` no longer hangs forever if a download dies.
  Waits up to 5 minutes with 3 retries, then returns EAGAIN (retry later).
- **Errno mapping**: Network errors → EHOSTUNREACH ("No route to host"),
  transient errors → EAGAIN ("Resource temporarily unavailable"), conflicts →
  EEXIST, disk full → ENOSPC. Users now see "No space left on device" instead
  of generic "Input/output error" for quota/disk issues.
- **Rclone error parsing**: Extracts human-readable messages from rclone's JSON
  log format. Auth errors (invalid_grant, token expired/revoked) map to
  "Permission denied". Timeout/DNS → "Host unreachable".
- **Poller backoff**: Exponential backoff after 3 consecutive failures (doubles
  interval, caps at 10 min). After 10 failures, logs ERROR with actionable
  guidance instead of spamming identical warnings.
- **Upload queue resilience**: On loop exit, resets stuck Uploading inodes to
  Dirty and logs ERROR. Previously, writes silently stopped syncing.
- **Rclone process cleanup**: `kill_on_drop` ensures timed-out rclone processes
  are killed (not orphaned). 256MB output cap prevents OOM from huge listings.
- **Batch chunking**: DB operations process in chunks of 1000, releasing the
  mutex between chunks so FUSE operations aren't starved during large polls.
- **QuotaExceeded is now retryable** with exponential backoff.

## [0.5.0] - 2026-04-07

### Performance
- **Async mkdir and rename**: Both now return instantly (<1ms). Remote operations
  run in background tasks. Previously 200-1000ms per call.
- **Deferred hydration**: `open()` returns immediately for uncached files. The
  download starts in the background; `read()` blocks only if data isn't ready yet.
  Enables parallel downloads when opening multiple files.
- **Prefetch child directories**: When a directory is first listed, its
  subdirectories are populated in the background (4 concurrent). Next `cd`
  into a child is instant.
- **Stop invalidating dir_listed on local writes**: `touch file; ls` no longer
  re-fetches the directory from rclone. Local writes update the DB directly;
  the poller handles remote changes independently.
- **Increase attr/entry timeout**: 5s → 60s. Kernel caches directory entries
  and file attributes 12x longer, reducing FUSE→daemon queries.

## [0.4.0] - 2026-04-06

### Added
- **ETag-based poll diffing**: The poller now diffs the remote listing against a
  DB snapshot in memory. Only entries that actually changed (new, modified, or
  deleted) trigger DB writes. Idle polls with 2793 files go from 2793 upserts
  to 1 SELECT + 1 batch UPDATE.
- **Remote deletion detection**: Entries absent from the remote listing are
  automatically removed from the DB (unless dirty/uploading). Previously,
  remote deletions were invisible to the daemon.
- `--hash` flag on `rclone lsjson --recursive` for content-based change
  detection via ETags (MD5/SHA-1).
- New migration `0003_poll_generation` adding generation counter to file_index.
- `StateDb::snapshot_remote_index`, `batch_mark_generation`,
  `delete_stale_entries`, `get/set_poll_generation`.
- `MockBackend::remove_file()` and `modify_file()` for simulating remote changes.
- 10 new tests for poll diffing behavior.

## [0.3.0] - 2026-04-04

### Added
- **Delete tombstones**: `rm` and `rm -rf` now work reliably. When files are
  deleted locally, a tombstone record prevents the poller from re-adding them
  before the background remote delete completes. Directory tombstones also
  block children (prefix match). Tombstones expire after 5 minutes as a safety
  net and are cleaned up each poll cycle.
- New migration `0002_delete_tombstones` with `delete_tombstones` table
- 9 new tombstone tests

## [0.2.1] - 2026-04-03

### Security
- **Path traversal**: `validate_filename()` rejects `..`, `/`, null bytes in all
  FUSE create/mkdir/rename handlers. `safe_cache_path()` canonicalizes paths and
  verifies they stay within the cache directory.
- **Symlink attacks**: Upload queue rejects symlink cache files via
  `symlink_metadata()`. `.meta/partial` directory set to 0o700. Hydration temp
  files use random suffixes to defeat symlink races.
- **DoS protection**: Poller rejects remote listings exceeding 500,000 entries.
- **Input sanitization**: `populate_directory` and poller skip entries with `..`
  or null bytes in filenames from remote listings.

### Added
- 5 security regression tests (path traversal, symlink detection, SQL injection,
  integer boundaries)

## [0.2.0] - 2026-04-03

First functional release. The daemon mounts a Google Drive (or any rclone remote)
as a FUSE filesystem with bidirectional sync.

### Added
- `setattr` support for file truncation (`>` redirection on existing files)
- `Backend::rmdir()` trait method using `rclone rmdir` for directory removal
- `StateDb::insert_root()` for reliable root inode creation at inode 1
- `StateDb::batch_upsert_remote_files()` for transactional directory population
- `StateDb::set_dirty_size()` to track file size after writes
- `StateDb::delete_mount_entries()` for clearing corrupt state on startup
- Non-blocking `unlink`/`rmdir` — remote deletes run in background tokio tasks
- Clean shutdown via `fusermount3 -u` on ctrl-c
- Remote poller resolves parent inodes from path hierarchy (not all files at root)
- 29 functional tests covering multi-component workflows
- CLAUDE.md with build commands and architecture overview

### Fixed
- **FUSE thread panic**: `Handle::current()` called from a `std::thread` with no
  tokio runtime context. Now the Handle is captured in the main thread and passed in.
- **readdir EIO**: `reply.ok()` was not called before returning when the FUSE reply
  buffer was full. Fuser's `Drop` impl sent `-EIO` for unsent replies.
- **Files read as empty**: `handle_write` updated the cache file but never updated
  the file size in the DB. `getattr` reported size 0, so the kernel never read data.
- **Duplicate directory entries**: `join_remote("/", name)` produced `/name` but
  rclone returns `name` (no leading slash). Paths now consistently omit leading slashes.
- **Wrong delete paths**: `populate_directory` stored rclone's relative paths instead
  of full paths from root. Deletes targeted wrong remote locations.
- **Rename lost file content**: `rename_entry` didn't update `cache_path` in the DB.
  After rename, `open()` tried the old (missing) cache path.
- **Rename failed on new files**: `rclone moveto` was called on Dirty files that
  hadn't been uploaded yet. Now only remotely-existing files trigger remote renames.
- **rmdir used wrong rclone command**: `deletefile` (for files) was used instead of
  `rmdir` (for directories).
- **Daemon hung on shutdown**: `fuser::mount2` blocks until unmount, but nothing
  triggered unmount on ctrl-c.
- **inotify watcher died immediately**: `FsWatcher` was dropped right after creation
  because the return value wasn't stored.
- **Root inode corruption**: Self-referencing FK (`parent_inode=1` on inode 1) failed
  when autoincrement advanced past 1. Root now uses explicit inode=1 with NULL parent.
- **Startup race**: Poller started before root inode existed, causing FK violations.
  Root creation now happens before any background tasks start.
- **Cache/mount dirs missing at startup**: Watcher and FUSE mount failed because
  directories weren't created until later in the startup sequence.
- **NULL parent_inode crash**: `row_to_entry` read parent as non-nullable after root
  was changed to NULL parent.
- **Silent error swallowing**: ~20 instances of `let _ = result` across the daemon
  replaced with `warn!` logging. Two intentional cases (oneshot senders) documented.
- **Poller flattened hierarchy**: All 2793 files were inserted with `parent=root`.
  Now resolves parent inodes from path components.
- **rclone "doesn't exist" mapped to Fatal**: Stderr containing "doesn't exist" or
  "404" now correctly maps to `NotFound`.

### Changed
- README Quick Start updated to match actual CLI commands
- `handle_unlink`/`handle_rmdir` delete DB entries immediately, queue remote
  deletes asynchronously
- `populate_directory` uses `batch_upsert_remote_files` (single transaction)
  instead of individual upserts

## [0.1.0] - 2026-03-30

### Added
- Initial implementation of Phase 1 (read-only VFS) and Phase 2 (bidirectional sync)
- FUSE filesystem: lookup, getattr, readdir, open, read, write, create, mkdir,
  unlink, rmdir, rename, fsync
- SQLite state database with WAL mode, migrations, file index, sync queue, cache LRU
- rclone backend: lsjson, copyto, deletefile, mkdir, moveto, about
- Upload queue with debounce and concurrency control
- Remote poller (polling-based change detection)
- Cache manager with LRU eviction and configurable quota
- inotify watcher for local change detection
- Conflict resolver with `.conflict.{ts}.{hash}` naming
- CLI: status, ls, config (show/test/edit), conflicts, pin/unpin (stubs)
- Systemd user service unit
- Install script with prerequisite checks
