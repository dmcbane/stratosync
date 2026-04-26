# Configuration

Configuration lives at `~/.config/stratosync/config.toml` (XDG_CONFIG_HOME).

## Full Example

```toml
# stratosync configuration

[daemon]
log_level  = "info"          # trace | debug | info | warn | error
log_file   = ""              # "" = stderr; or absolute path
pid_file   = ""              # "" = XDG_RUNTIME_DIR/stratosync.pid
socket     = ""              # "" = XDG_RUNTIME_DIR/stratosync.sock (CLI comms)

[daemon.fuse]
threads         = 4          # FUSE thread pool size
attr_timeout_s  = 5          # kernel attribute cache TTL
entry_timeout_s = 5          # kernel entry cache TTL
allow_other     = false      # allow other users to access the mount
                             # requires user_allow_other in /etc/fuse.conf

[daemon.sync]
max_hydration_concurrent = 4
max_upload_concurrent    = 2
upload_debounce_ms       = 2000   # wait 2s after last write before uploading
upload_close_debounce_ms = 500    # wait 0.5s after close
text_conflict_strategy   = "keep_both"    # or "merge"
text_extensions          = ["md", "txt", "rs", "py", "toml", "yaml", "json"]

[daemon.metrics]
enabled     = false                # opt-in; serve GET /metrics when true
listen_addr = "127.0.0.1:9090"     # host:port for the Prometheus endpoint

# ─────────────────────────────────────────────
# Mount configurations — one [[mount]] per sync target
# ─────────────────────────────────────────────

[[mount]]
name        = "gdrive"
remote      = "gdrive:/"          # rclone remote:path
mount_path  = "~/GoogleDrive"
cache_quota = "10 GiB"
poll_interval = "30s"
enabled     = true

# Selective sync: glob patterns to exclude from indexing/upload/FUSE create.
# Optional; default is no patterns (full sync).
ignore_patterns = [
    "*.tmp",
    "*.log",
    "node_modules/**",
    "**/target",
]

# Bandwidth schedule: only dispatch uploads during this local-time window.
# Optional; omit for 24/7 uploads. Wraparound supported (e.g. 22:00-06:00
# means "evenings and overnight"). fsync() always bypasses this gate.
upload_window = "22:00-06:00"

[mount.rclone]
# Extra flags passed to every rclone invocation for this mount
extra_flags  = []
bwlimit      = ""        # e.g. "10M" for 10 MB/s
transfers    = 4
checkers     = 8
log_level    = "ERROR"   # suppress rclone's own output by default

[mount.eviction]
policy     = "lru"       # lru | lfu | size_desc | never
low_mark   = "80%"       # evict until at this % of quota
high_mark  = "90%"       # start evicting at this % of quota

# ─────────────────────────────────────────────

[[mount]]
name        = "onedrive"
remote      = "onedrive:/"
mount_path  = "~/OneDrive"
cache_quota = "5 GiB"
poll_interval = "30s"
enabled     = true

[mount.rclone]
extra_flags = ["--onedrive-chunk-size", "10M"]
bwlimit     = ""
transfers   = 4
checkers    = 8

# ─────────────────────────────────────────────

[[mount]]
name        = "photos-s3"
remote      = "s3:my-photos-bucket/"
mount_path  = "~/Photos-S3"
cache_quota = "50 GiB"
poll_interval = "120s"    # S3 polling is more expensive
enabled     = false       # disabled by default

[mount.rclone]
extra_flags = ["--s3-acl", "private"]
```

---

## Bandwidth schedule (`upload_window`)

Per-mount local-time window during which uploads are permitted.
Optional — omit for 24/7 uploads (the existing default behavior).

```toml
[[mount]]
name = "drive"
# ...
upload_window = "22:00-06:00"   # only upload between 10pm and 6am
```

**Format**: `"HH:MM-HH:MM"`, 24-hour. Local time (wall clock), so users
can express schedules in terms they actually live by. Wraparound past
midnight is supported (start later than end). The end time is exclusive
(`06:00` means uploads stop just before 06:00).

**Behavior**:

- Outside the window, queued uploads are held — the loop sleeps until
  the window opens rather than busy-checking. New writes still enqueue,
  they just don't dispatch.
- In-flight uploads are **not interrupted** when the window closes.
  Cancelling mid-upload risks corrupted partial state on the remote;
  letting them finish is safer and the bandwidth difference is small.
- `fsync()` **always bypasses** the schedule. The user explicitly asked
  for durability; the bandwidth policy is coarse and shouldn't override
  that.
- Pinning, polling, and hydration are **not gated** — only uploads.
  Reading from the remote on demand always works.
- The degenerate `"HH:MM-HH:MM"` (start equals end) is treated as
  always-open, not always-closed. This is the less-surprising
  interpretation for a typo.

**Limitations** (intentional, room for follow-up):

- A single window per mount. No multi-window schedules ("9–12 and
  14–17") yet. Stack two mounts of the same remote if you need that.
- No day-of-week selectors. Always applied.
- No bandwidth-rate limiting (Mbps caps). Use `[mount.rclone] bwlimit`
  for that — rclone handles it inside each upload.

---

## Metrics endpoint (`[daemon.metrics]`)

Optional Prometheus-compatible HTTP endpoint. Off by default — opt in
when you actually want a scraper to point at the daemon.

```toml
[daemon.metrics]
enabled     = true
listen_addr = "127.0.0.1:9090"
```

Exposed metrics (all gauges; one series per mount unless noted):

| Name | Description |
|------|-------------|
| `stratosync_build_info{version}` | Always 1; `version` label carries the daemon version. |
| `stratosync_daemon_uptime_seconds` | Seconds since daemon start. |
| `stratosync_daemon_pid` | Process ID. |
| `stratosync_mounts_total` | Number of configured mounts. |
| `stratosync_mount_cache_used_bytes{mount}` | Bytes in the local cache. |
| `stratosync_mount_cache_quota_bytes{mount}` | Configured cache quota. |
| `stratosync_mount_pinned_files{mount}` | Files pinned for offline use. |
| `stratosync_mount_upload_queue_pending{mount}` | Files waiting to upload. |
| `stratosync_mount_upload_queue_in_flight{mount}` | Files currently uploading. |
| `stratosync_mount_hydration_active{mount}` | Files currently downloading. |
| `stratosync_mount_hydration_waiters{mount}` | `open()` callers blocked on a hydration. |
| `stratosync_mount_conflicts{mount}` | Conflict files on this mount. |
| `stratosync_mount_poller_consecutive_failures{mount}` | Poll failures since last success. |
| `stratosync_mount_poller_interval_seconds{mount}` | Current poll interval (may exceed configured value during backoff). |
| `stratosync_mount_poller_last_poll_timestamp_seconds{mount}` | Unix seconds of last successful poll; absent until the first poll. |

The endpoint is hand-rolled HTTP/1.1 on `tokio::net` — no axum/hyper
dependency added. `GET /metrics` returns the exposition format; any
other path returns 404. Bind to `127.0.0.1` unless you've put the daemon
behind firewall rules; the metrics include path counts that can leak
information about your file structure to anyone scraping.

---

## Selective sync (`ignore_patterns`)

Per-mount glob patterns that exclude matching paths from three places at
once: the remote poller's index, FUSE `create`/`mkdir` operations, and the
inotify-driven upload queue.

```toml
[[mount]]
name = "drive"
# ...
ignore_patterns = ["*.tmp", "node_modules/**", "*.log"]
```

**Match semantics** (via the `globset` crate's defaults):

- Patterns match the **remote path relative to the mount root**, with `/`
  separators and case-sensitive comparisons.
- `*` matches across path separators, so `*.log` catches `foo.log`,
  `dir/foo.log`, and `a/b/c/foo.log` alike. This is more permissive than
  shell globs but matches what most users expect from a sync tool.
- Use `dir/**` to ignore the *contents* of a subtree. The directory entry
  itself is unaffected — to also block creation of the directory, add the
  bare `dir` pattern as well.
- Patterns are validated at daemon startup; a malformed pattern aborts
  startup with an error naming the offending pattern.

**Behavior on each ingress point**:

| Where               | Effect                                                                 |
|---------------------|------------------------------------------------------------------------|
| Remote poller       | Skips upsert; matching entries never enter `file_index`.              |
| FUSE `create`/`mkdir` | Returns `EPERM` so applications see a clear error, not silent loss.  |
| inotify watcher     | Skips enqueue; cache writes to ignored paths never become uploads.    |

**Reversibility — preserve-on-ignore**: an entry already in the DB that
newly matches a pattern is *preserved*, not retroactively wiped. The
poller's stale-entry sweep is told to keep these entries by bumping their
generation. Net effect: ignore patterns prevent new indexing; they never
unindex. Remove the pattern and the file behaves normally again. Cleaning
up entries that retroactively match an ignore pattern is a manual step
(future work: a `stratosync ignore prune` CLI command).

**Limitations** (intentional, room for follow-up):

- No in-tree `.stratosyncignore` files (cascading per-directory rules).
- No negation patterns (`!keep.tmp`).
- No retroactive cleanup of entries that match a newly-added pattern.

---

## Configuration Loading

```rust
pub struct Config {
    pub daemon: DaemonConfig,
    pub mounts: Vec<MountConfig>,
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = xdg_config_home()
            .join("stratosync")
            .join("config.toml");
        
        let content = fs::read_to_string(&path)
            .map_err(|_| ConfigError::NotFound(path.clone()))?;
        
        let raw: RawConfig = toml::from_str(&content)?;
        raw.validate_and_build()
    }
}
```

Validation checks:
- Mount paths don't overlap with each other
- `cache_quota` is parseable and > 100 MiB
- `poll_interval` is parseable and > 5s
- rclone binary is on PATH
- All `remote` values are valid rclone remote specs

---

## CLI-driven Configuration

The `stratosync config` subcommand provides a guided setup:

```bash
# Interactive wizard: configure a new mount
stratosync config add-mount

# Show current configuration
stratosync config show

# Edit raw config in $EDITOR
stratosync config edit

# Test rclone connectivity for all configured remotes
stratosync config test
```

`add-mount` calls `rclone config` to handle OAuth flows, then writes the mount block to `config.toml`. This keeps us out of the OAuth token management business entirely.

---

## Environment Variable Overrides

For CI/testing and Docker deployments:

```
STRATOSYNC_LOG_LEVEL=debug
STRATOSYNC_CONFIG=/path/to/config.toml
STRATOSYNC_RCLONE=/path/to/rclone
```
