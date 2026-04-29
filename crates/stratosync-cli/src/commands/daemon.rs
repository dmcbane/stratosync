// daemon.rs — thin wrappers over `systemctl --user` / `journalctl --user`
// for the stratosyncd user service. Saves end users from typing the long
// systemd invocations.
use std::process::Command;

use anyhow::{bail, Context, Result};

const UNIT: &str = "stratosyncd.service";

fn ensure_systemctl() -> Result<()> {
    if which("systemctl").is_none() {
        bail!("systemctl not found — this command requires a systemd-based system");
    }
    Ok(())
}

fn which(prog: &str) -> Option<std::path::PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).find_map(|dir| {
            let candidate = dir.join(prog);
            candidate.is_file().then_some(candidate)
        })
    })
}

fn run_systemctl(args: &[&str]) -> Result<()> {
    ensure_systemctl()?;
    let status = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .arg(UNIT)
        .status()
        .context("failed to invoke systemctl")?;
    if !status.success() {
        bail!("systemctl --user {} {UNIT} exited with status {}", args.join(" "), status);
    }
    Ok(())
}

pub fn start() -> Result<()> {
    run_systemctl(&["start"])
}

pub fn stop() -> Result<()> {
    run_systemctl(&["stop"])
}

pub fn restart() -> Result<()> {
    run_systemctl(&["restart"])
}

pub fn reload() -> Result<()> {
    // `try-reload-or-restart` is friendlier than plain `reload` because the
    // unit may not declare ExecReload; in that case systemd falls back to
    // a restart, which is what most users want.
    run_systemctl(&["try-reload-or-restart"])
}

pub fn status() -> Result<()> {
    // `status` exits non-zero when the unit is inactive; that's informational,
    // not an error from the user's perspective. Show the output and don't
    // bubble the non-zero exit up as an anyhow error.
    ensure_systemctl()?;
    let status = Command::new("systemctl")
        .arg("--user")
        .arg("status")
        .arg(UNIT)
        .status()
        .context("failed to invoke systemctl")?;
    // 0 = active, 3 = inactive/dead, 4 = no such unit. Surface 4 as an error
    // (the user almost certainly hasn't installed the unit file); silently
    // accept the rest so `daemon status` matches systemctl semantics.
    if let Some(4) = status.code() {
        bail!("unit {UNIT} not found — install it with `daemon install` or run install.sh");
    }
    Ok(())
}

pub fn enable(now: bool) -> Result<()> {
    if now {
        run_systemctl(&["enable", "--now"])
    } else {
        run_systemctl(&["enable"])
    }
}

pub fn disable(now: bool) -> Result<()> {
    if now {
        run_systemctl(&["disable", "--now"])
    } else {
        run_systemctl(&["disable"])
    }
}

pub fn logs(follow: bool, lines: Option<u32>) -> Result<()> {
    if which("journalctl").is_none() {
        bail!("journalctl not found — this command requires a systemd-based system");
    }
    // Use `--user-unit=<unit>` rather than `--user -u <unit>`. The latter
    // reads from the per-user journal file, which doesn't exist when
    // journald has no persistent storage (`/var/log/journal` absent).
    // `--user-unit=` filters the system journal by `_SYSTEMD_USER_UNIT=`,
    // which works in both volatile and persistent modes.
    let mut cmd = Command::new("journalctl");
    cmd.arg(format!("--user-unit={UNIT}"));
    if follow {
        cmd.arg("-f");
    }
    if let Some(n) = lines {
        cmd.arg("-n").arg(n.to_string());
    } else if !follow {
        // Default: show the last 200 lines instead of the entire journal,
        // which can span weeks.
        cmd.arg("-n").arg("200");
    }
    let status = cmd.status().context("failed to invoke journalctl")?;
    // journalctl returns 130 when the user hits Ctrl-C during --follow; that's
    // the normal exit path and shouldn't surface as an error.
    if !status.success() && status.code() != Some(130) {
        bail!("journalctl exited with status {}", status);
    }
    Ok(())
}
