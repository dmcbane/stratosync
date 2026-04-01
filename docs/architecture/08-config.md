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
