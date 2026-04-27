# Usage Guide

This guide covers what stratosync looks like in daily use: filesystem operations, every CLI command, and the per-mount features that turn it from "rclone but FUSE" into something you can leave running.

For installation, see **[installation.md](installation.md)**. For every TOML knob, see **[architecture/08-config.md](architecture/08-config.md)**.

---

## Filesystem operations

Once a mount is up, the cloud directory works like any local directory:

```bash
# Browse — directory listing comes from the local index, no network call.
ls ~/GoogleDrive/Documents/

# Read — file is fetched from the cloud on first open(), then cached.
cat ~/GoogleDrive/Documents/report.pdf

# Edit — writes go to the local cache instantly; upload runs in the background.
vim ~/GoogleDrive/Documents/notes.md

# Create / copy / move / delete — all work.
cp local-file.txt ~/GoogleDrive/
mv ~/GoogleDrive/old.txt ~/GoogleDrive/new.txt
rm ~/GoogleDrive/temp.txt
mkdir ~/GoogleDrive/NewFolder
```

### What's happening underneath

| You did | Daemon did |
|---------|------------|
| `ls ~/Drive/` | Served the listing from `file_index` (SQLite). No backend call unless this directory has never been listed. |
| `cat ~/Drive/foo.pdf` (cache miss) | Marked the inode `Hydrating`, ran `rclone copy` to local cache, parked the FUSE thread until done, then served `read()`. |
| `cat ~/Drive/foo.pdf` (cache hit) | Direct `pread()` from the local cache file. No daemon logic. |
| `vim ~/Drive/notes.md` (save) | Wrote to the cache, marked the inode `Dirty`, scheduled an upload 2 s out (debounced). On `:fsync` the upload fires immediately. |
| `mkdir ~/Drive/NewFolder` | Updated the local DB, queued an async `rclone mkdir`. The directory exists locally before the remote call returns. |

---

## CLI reference

The CLI binary is `stratosync`. The daemon is a separate binary `stratosyncd`. There is no `stratosync init` or `stratosync daemon` subcommand.

```text
stratosync status                       Sync status across all mounts
stratosync ls [path]                    List remote contents
stratosync dashboard [--snapshot]       Live TUI of daemon state
stratosync config show|test|edit        Configuration management
stratosync pin <path>                   Pin file/dir for offline use
stratosync unpin <path>                 Remove an offline pin
stratosync conflicts [SUB]              List/resolve conflict files
stratosync versions list <path>         List recorded file versions
stratosync versions restore <path> --index N
                                        Restore a recorded version
stratosync completion                   Print shell-completion setup
```

Each is described below.

### `status` — one-shot sync overview

```text
$ stratosync status
gdrive
  cache:    2.4 GiB / 10.0 GiB    pinned: 12 files
  pending:  3 files               in-flight: 1 upload
  conflicts: 0                    last poll: 18s ago
```

Reads the per-mount SQLite DBs directly; works even if the daemon is down.

### `ls` — remote-aware listing

```bash
stratosync ls ~/GoogleDrive/Documents
```

Like `ls`, but also shows sync status (cached / dirty / remote / conflict) per entry. Useful when you want to see what's local without `getfattr`.

### `dashboard` — live TUI

```bash
stratosync dashboard            # ratatui-based live view
stratosync dashboard --snapshot # plain-text snapshot, prints once and exits
```

Talks to the daemon over the IPC socket and shows per-mount queues, hydration state, and poller status updating in real time. The `--snapshot` form is good for `watch` or for piping into `grep`.

### `config` — manage `~/.config/stratosync/config.toml`

```bash
stratosync config show     # pretty-printed current config
stratosync config test     # verify rclone connectivity for every enabled mount
stratosync config edit     # open in $EDITOR, validate on save
```

`config test` is the right command to run after editing; it catches typos in
`remote` names or unreachable backends before the daemon notices.

### `pin` / `unpin` — offline availability

```bash
stratosync pin ~/GoogleDrive/important.pdf      # single file
stratosync pin ~/GoogleDrive/WorkDocs/          # whole directory, recursive
stratosync unpin ~/GoogleDrive/important.pdf
```

Pinned files are eagerly hydrated and excluded from LRU eviction. Pin state
persists across daemon restarts. `stratosync status` shows the pinned count.

### `conflicts` — see and resolve conflict siblings

```bash
stratosync conflicts                                     # list everything
stratosync conflicts cleanup [--dry-run]                 # drop byte-equal siblings
stratosync conflicts keep-local  ~/Drive/path/file.md    # keep your version
stratosync conflicts keep-remote ~/Drive/path/file.md    # keep remote version
stratosync conflicts merge       ~/Drive/path/file.md    # 3-way merge via git merge-file
stratosync conflicts diff        ~/Drive/path/file.md    # unified diff
```

Conflict siblings live under `.stratosync-conflicts/` on the remote, so they
don't pollute the user-visible namespace. `cleanup` is the post-incident
sweeper: it removes conflict files whose content already matches the canonical
version (the common case after a transient false-positive). Use `--dry-run`
first.

### `versions` — file history

```bash
stratosync versions list    ~/Drive/notes.md
stratosync versions restore ~/Drive/notes.md --index 2
```

Stratosync snapshots file content at two moments where data could otherwise be
lost: just before a poller-detected remote change overwrites your local cache
(`before_poll`), and right after a successful upload (`after_upload`). Snapshots
are content-addressed and deduplicated against the 3-way-merge base store.
Disable per-mount with `version_retention = 0`. See [Versioning](#file-versioning)
below for the full picture.

### `completion` — shell completions

```bash
stratosync completion          # prints setup instructions for bash/zsh/fish
```

---

## Selective sync

Per-mount glob patterns that exclude paths from indexing, FUSE creation, and
upload — exactly the file-clutter rejection you'd expect:

```toml
[[mount]]
name = "drive"
# ...
ignore_patterns = [
    "*.tmp",
    "*.log",
    "node_modules/**",
    "**/target",
    ".cache/**",
]
```

**What gets blocked:**

| Where it kicks in | Effect |
|-------------------|--------|
| Remote poller | Matching paths never enter the local index. |
| FUSE `create` / `mkdir` | Returns `EPERM` so apps see a clear error rather than silent loss. |
| inotify watcher | Cache writes to ignored paths never become uploads. |

**Match semantics** (via the `globset` crate):

- Patterns match the **remote path relative to the mount root**, with `/`
  separators and case-sensitive comparisons.
- `*` matches across path separators — `*.log` catches `foo.log`, `dir/foo.log`,
  and `a/b/c/foo.log` alike. This differs from shell globs, but matches what
  most users expect from a sync tool.
- Use `dir/**` to ignore the *contents* of a subtree. To also block creation
  of the directory itself, add the bare `dir` pattern.
- Patterns are validated at daemon startup; a malformed pattern aborts startup
  with an error naming the offending pattern.

**Reversibility**: an entry already in the DB that newly matches a pattern is
*preserved*, not retroactively wiped. Rules prevent new indexing; they never
unindex. Remove the pattern and the file behaves normally again.

**Limitations** (room for future work):

- No in-tree `.stratosyncignore` files (cascading per-directory rules).
- No negation patterns (`!keep.tmp`).
- No retroactive cleanup of entries matching a newly-added pattern.

---

## Bandwidth scheduling

Restrict uploads to a local-time window — perfect for metered connections or
shared upstream pipes:

```toml
[[mount]]
name = "drive"
# ...
upload_window = "22:00-06:00"   # only upload between 10pm and 6am
```

- 24-hour `HH:MM-HH:MM`, **local time**. Wraparound past midnight is supported.
- Outside the window, queued uploads are *held* — the dispatcher sleeps until
  the window opens; no busy-wait.
- In-flight uploads are not interrupted when the window closes (cancelling
  mid-upload risks corrupted partial state).
- `fsync()` **always bypasses** the schedule. The user explicitly asked for
  durability.
- Polling, hydration, and pinning are not gated; only uploads are.

For Mbps caps (rather than time windows), use `[mount.rclone] bwlimit = "10M"`.

---

## File versioning

Stratosync keeps the last N versions of each file per mount, captured at the
two moments a sync tool can quietly lose data:

1. **`before_poll`** — the poller has detected a remote change and is about to
   invalidate your cached local content. The old content is snapshotted first.
2. **`after_upload`** — your local upload just succeeded; the uploaded content
   is snapshotted, so you can roll back if a later edit was wrong.

```toml
[[mount]]
# ...
version_retention = 10        # default; 0 disables
```

Snapshots reuse the content-addressed `BaseStore` already on disk for the
3-way-merge feature, so identical content (e.g. an unchanged poll cycle)
deduplicates to a single blob. Files larger than `[daemon.sync] base_max_file_size`
(default 10 MiB) are skipped — the version cache is for documents and source
code, not media archives.

```bash
$ stratosync versions list ~/Drive/notes.md
  #  recorded             size       source         hash
  0  2026-04-26 14:02:11  12.3 KB    after_upload   a1b2c3d4e5f6
  1  2026-04-26 11:18:03  12.1 KB    before_poll    998877eeddcc
  2  2026-04-25 22:44:17  11.9 KB    after_upload   ff00aa11bb22

$ stratosync versions restore ~/Drive/notes.md --index 2
Restored version #2 of '/Drive/notes.md' (recorded 2026-04-25 22:44:17).
Marked Dirty — will upload on next sync window.
```

`restore` copies the historical blob back over the cache file and marks the
entry `Dirty`, so the next sync uploads it as the new canonical version.

**Limitations** (CLI-only for now):

- No FUSE-visible `.versions/` shadow tree — see [ROADMAP.md](ROADMAP.md) for
  the planned filesystem view.
- No bulk restore across a directory.
- No diff between two versions (restore one, then run your usual diff tool).

---

## xattrs — sync metadata for any tool

Every file in a stratosync mount carries three read-only extended attributes.
Read them with `getfattr` or any tool that speaks xattrs:

```bash
$ getfattr -n user.stratosync.status      ~/Drive/report.pdf
user.stratosync.status="cached"

$ getfattr -n user.stratosync.etag        ~/Drive/report.pdf
user.stratosync.etag="abc123def456..."

$ getfattr -n user.stratosync.remote_path ~/Drive/report.pdf
user.stratosync.remote_path="/Documents/report.pdf"
```

The Nautilus emblem extension and the `stratosync-tray` indicator both read
these xattrs to render status without talking to the daemon. `setxattr` and
`removexattr` return `ENOTSUP` — the attributes are read-only.

---

## Prometheus `/metrics` endpoint

Optional, off by default. Opt in via:

```toml
[daemon.metrics]
enabled     = true
listen_addr = "127.0.0.1:9090"
```

`GET /metrics` returns Prometheus exposition with per-mount cache, queue,
hydration, conflict, and poller gauges. See
[architecture/08-config.md](architecture/08-config.md#metrics-endpoint-daemonmetrics)
for the full series list.

> **Bind to localhost** unless you've put the daemon behind firewall rules:
> the per-mount labels reveal the mount names from your config, which can leak
> account-naming info to anyone who scrapes the endpoint.

---

## Systemd operations

Daily commands, for reference:

```bash
systemctl --user start stratosyncd       # start
systemctl --user stop stratosyncd        # stop (unmounts cleanly)
systemctl --user restart stratosyncd     # pick up config changes
systemctl --user status stratosyncd      # is it running?
journalctl --user -u stratosyncd -f      # follow live logs
journalctl --user -u stratosyncd --since "1 hour ago"
```

The daemon performs `fusermount3 -u <mount_path>` for every active mount on
clean shutdown. If a mount is stuck after a crash:

```bash
fusermount3 -u ~/GoogleDrive
```

---

## Troubleshooting

**The mount appears empty.** First poll hasn't completed yet — give it
`poll_interval` seconds. After that, `journalctl --user -u stratosyncd | tail`
will show backend errors (auth, network, quota).

**`Operation not permitted` on `mkdir` or `touch`.** Either `ignore_patterns`
matched the path, or the systemd unit has `NoNewPrivileges=true` re-enabled
(blocking the setuid `fusermount3` from mounting). Check both.

**`Resource temporarily unavailable` (`EAGAIN`) on `read()`.** A hydration
timed out (5-minute default with 3 retries). The daemon backs off; just retry
the operation. Persistent failures show up as poller errors in the logs.

**Conflicts piling up under `.stratosync-conflicts/`.** Run
`stratosync conflicts cleanup --dry-run` first; it auto-drops siblings that
are byte-equal to the canonical file. For real divergence, use
`stratosync conflicts merge|keep-local|keep-remote`.

**Cache full / `ENOSPC`.** LRU eviction runs at 90 % of `cache_quota`. If
you're consistently near quota, raise it or pin less aggressively.
