/// Desktop notification helpers (best-effort via `notify-send`).
///
/// Failure is expected on headless servers, non-GNOME desktops, and CI.
/// Errors are logged at debug level and never propagated.
use tracing::debug;

/// Send a desktop notification via `notify-send`.
pub fn send(title: &str, body: &str) {
    send_with_icon("dialog-warning", title, body);
}

fn send_with_icon(icon: &str, title: &str, body: &str) {
    match std::process::Command::new("notify-send")
        .args(["--urgency=normal", &format!("--icon={icon}"), title, body])
        .status()
    {
        Ok(s) if s.success() => debug!("desktop notification sent: {title}"),
        Ok(s) => debug!("notify-send exited with {s}"),
        Err(e) => debug!("notify-send unavailable: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_does_not_panic_when_notify_send_missing() {
        // notify-send may or may not be available; either way, no panic.
        send("test title", "test body");
    }
}
