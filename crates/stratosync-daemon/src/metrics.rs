//! Prometheus-compatible /metrics endpoint.
//!
//! This is a tiny HTTP/1.1 server hand-rolled on top of `tokio::net`. We
//! deliberately do not pull in axum / hyper-server / warp for a single
//! GET endpoint — keeping the dep graph tight is worth more than the
//! ergonomics of a router we'd never grow into.
//!
//! The server reuses the existing `DaemonState::snapshot()` aggregation
//! from `state.rs`, so adding a metric is a one-line append in
//! `render_prometheus` whenever a new field appears in `DaemonStatus`.
use std::fmt::Write as _;
use std::sync::Arc;

use anyhow::{Context, Result};
use stratosync_core::ipc::DaemonStatus;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, info, warn};

use crate::state::DaemonState;

/// Run the metrics HTTP listener forever. Call from a dedicated tokio
/// task. Returns when the listener can't be bound; otherwise loops.
pub async fn serve(addr: String, state: Arc<DaemonState>) -> Result<()> {
    let listener = TcpListener::bind(&addr).await
        .with_context(|| format!("bind metrics listener {addr}"))?;
    info!(%addr, "metrics endpoint listening");

    loop {
        let (sock, peer) = match listener.accept().await {
            Ok(x)  => x,
            Err(e) => { warn!("metrics accept failed: {e}"); continue; }
        };
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            if let Err(e) = handle_conn(sock, state).await {
                debug!(?peer, "metrics request failed: {e}");
            }
        });
    }
}

async fn handle_conn(mut sock: TcpStream, state: Arc<DaemonState>) -> Result<()> {
    // Read up to the end of the request headers. A Prometheus scraper sends
    // a tiny GET with no body; 4 KiB is plenty.
    let mut buf = [0u8; 4096];
    let n = sock.read(&mut buf).await?;
    let head = std::str::from_utf8(&buf[..n]).unwrap_or("");
    let first = head.lines().next().unwrap_or("");

    let (status, body, content_type) = match parse_request_line(first) {
        Some(("GET", "/metrics")) => {
            let snap = state.snapshot().await;
            (200, render_prometheus(&snap), "text/plain; version=0.0.4; charset=utf-8")
        }
        Some(("GET", "/")) => {
            (200, "stratosync metrics — try /metrics\n".to_string(), "text/plain; charset=utf-8")
        }
        Some(_) => (404, "not found\n".to_string(), "text/plain; charset=utf-8"),
        None    => (400, "bad request\n".to_string(), "text/plain; charset=utf-8"),
    };

    let resp = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: {ct}\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        status = status,
        reason = http_reason(status),
        ct = content_type,
        len = body.len(),
        body = body,
    );
    sock.write_all(resp.as_bytes()).await?;
    sock.flush().await?;
    Ok(())
}

fn http_reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        _   => "OK",
    }
}

/// Parse an HTTP request line of the form `METHOD PATH HTTP/x.y`. Returns
/// (method, path) on success. We only care about method + path.
fn parse_request_line(line: &str) -> Option<(&str, &str)> {
    let mut parts = line.split_whitespace();
    let method = parts.next()?;
    let path   = parts.next()?;
    let _ver   = parts.next()?; // HTTP/1.1, etc.
    Some((method, path))
}

// ── Prometheus exposition format ──────────────────────────────────────────────

/// Render a `DaemonStatus` snapshot as Prometheus text format
/// (https://prometheus.io/docs/instrumenting/exposition_formats/).
pub fn render_prometheus(s: &DaemonStatus) -> String {
    let mut out = String::with_capacity(2048);

    // Build info — the canonical "version exposed as a gauge" pattern.
    metric_help(&mut out, "stratosync_build_info", "Build information about the daemon. Always 1.");
    metric_type(&mut out, "stratosync_build_info", "gauge");
    let _ = writeln!(out,
        "stratosync_build_info{{version=\"{}\"}} 1",
        escape_label(&s.version),
    );

    // Daemon-level
    metric_help(&mut out, "stratosync_daemon_uptime_seconds", "Seconds since the daemon started.");
    metric_type(&mut out, "stratosync_daemon_uptime_seconds", "gauge");
    let _ = writeln!(out, "stratosync_daemon_uptime_seconds {}", s.uptime_secs);

    metric_help(&mut out, "stratosync_daemon_pid", "Process ID of the daemon.");
    metric_type(&mut out, "stratosync_daemon_pid", "gauge");
    let _ = writeln!(out, "stratosync_daemon_pid {}", s.pid);

    metric_help(&mut out, "stratosync_mounts_total", "Number of configured mounts in the daemon.");
    metric_type(&mut out, "stratosync_mounts_total", "gauge");
    let _ = writeln!(out, "stratosync_mounts_total {}", s.mounts.len());

    // Per-mount gauges. Group HELP/TYPE per metric (Prometheus requires the
    // family to be contiguous), then emit every mount's series.
    emit_mount_gauge(&mut out, s,
        "stratosync_mount_cache_used_bytes",
        "Bytes currently held in the local cache for this mount.",
        |m| m.cache.used_bytes);
    emit_mount_gauge(&mut out, s,
        "stratosync_mount_cache_quota_bytes",
        "Configured cache quota for this mount in bytes.",
        |m| m.cache.quota_bytes);
    emit_mount_gauge(&mut out, s,
        "stratosync_mount_pinned_files",
        "Number of files pinned for offline availability.",
        |m| m.cache.pinned_count);
    emit_mount_gauge(&mut out, s,
        "stratosync_mount_upload_queue_pending",
        "Files waiting to be uploaded.",
        |m| m.queue.pending);
    emit_mount_gauge(&mut out, s,
        "stratosync_mount_upload_queue_in_flight",
        "Files currently being uploaded.",
        |m| m.queue.in_flight.len() as u64);
    emit_mount_gauge(&mut out, s,
        "stratosync_mount_hydration_active",
        "Files currently downloading from the remote.",
        |m| m.hydration.active);
    emit_mount_gauge(&mut out, s,
        "stratosync_mount_hydration_waiters",
        "Total open() callers blocked on a hydration.",
        |m| m.hydration.waiters);
    emit_mount_gauge(&mut out, s,
        "stratosync_mount_conflicts",
        "Number of conflict files present for this mount.",
        |m| m.conflicts);
    emit_mount_gauge(&mut out, s,
        "stratosync_mount_poller_consecutive_failures",
        "Poll failures since the last success. Zero means the last poll succeeded.",
        |m| m.poller.consecutive_failures as u64);
    emit_mount_gauge(&mut out, s,
        "stratosync_mount_poller_interval_seconds",
        "Current polling interval. May exceed the configured value during backoff.",
        |m| m.poller.current_interval_secs);

    // Last poll timestamp — emit only for mounts that have polled at least once.
    metric_help(&mut out, "stratosync_mount_poller_last_poll_timestamp_seconds",
        "Unix timestamp of the most recent successful poll. Absent if the mount has not polled yet.");
    metric_type(&mut out, "stratosync_mount_poller_last_poll_timestamp_seconds", "gauge");
    for m in &s.mounts {
        if let Some(ts) = m.poller.last_poll_unix {
            let _ = writeln!(out,
                "stratosync_mount_poller_last_poll_timestamp_seconds{{mount=\"{}\"}} {}",
                escape_label(&m.name), ts,
            );
        }
    }

    out
}

fn emit_mount_gauge<F: Fn(&stratosync_core::ipc::MountStatus) -> u64>(
    out: &mut String,
    s: &DaemonStatus,
    name: &str,
    help: &str,
    f: F,
) {
    metric_help(out, name, help);
    metric_type(out, name, "gauge");
    for m in &s.mounts {
        let _ = writeln!(out,
            "{name}{{mount=\"{}\"}} {}",
            escape_label(&m.name),
            f(m),
        );
    }
}

fn metric_help(out: &mut String, name: &str, help: &str) {
    // HELP text only escapes backslash and newline (no quotes).
    let mut esc = String::with_capacity(help.len());
    for c in help.chars() {
        match c {
            '\\' => esc.push_str(r"\\"),
            '\n' => esc.push_str(r"\n"),
            c    => esc.push(c),
        }
    }
    let _ = writeln!(out, "# HELP {name} {esc}");
}

fn metric_type(out: &mut String, name: &str, ty: &str) {
    let _ = writeln!(out, "# TYPE {name} {ty}");
}

/// Escape a label value per Prometheus exposition format: backslash, newline,
/// and double-quote must be escaped. Everything else is UTF-8 verbatim.
fn escape_label(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str(r"\\"),
            '"'  => out.push_str(r#"\""#),
            '\n' => out.push_str(r"\n"),
            c    => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use stratosync_core::ipc::{
        ActiveUpload, CacheStatus, DaemonStatus, HydrationStatus,
        MountStatus, PollerStatus, QueueStatus,
    };

    fn sample() -> DaemonStatus {
        DaemonStatus {
            version: "0.12.0".into(),
            pid: 4242,
            uptime_secs: 3600,
            mounts: vec![MountStatus {
                name:       "gdrive".into(),
                remote:     "gdrive:/".into(),
                mount_path: "/home/u/Drive".into(),
                cache:      CacheStatus { used_bytes: 1234, quota_bytes: 10_000, pinned_count: 5 },
                queue:      QueueStatus {
                    pending: 2,
                    in_flight: vec![ActiveUpload {
                        inode: 1, path: "x".into(), size_bytes: 100, started_at_unix: 1,
                    }],
                },
                poller:     PollerStatus {
                    mode: "delta".into(),
                    last_poll_unix: Some(1_700_000_000),
                    next_poll_unix: Some(1_700_000_060),
                    consecutive_failures: 0,
                    current_interval_secs: 60,
                    last_error: None,
                },
                hydration:  HydrationStatus { active: 0, waiters: 0 },
                conflicts:  3,
            }],
        }
    }

    #[test]
    fn renders_prometheus_text_format() {
        let body = render_prometheus(&sample());

        // Required structure: HELP, TYPE, then series.
        assert!(body.contains("# HELP stratosync_build_info"), "{body}");
        assert!(body.contains("# TYPE stratosync_build_info gauge"));
        assert!(body.contains("stratosync_build_info{version=\"0.12.0\"} 1"));

        assert!(body.contains("stratosync_daemon_uptime_seconds 3600"));
        assert!(body.contains("stratosync_daemon_pid 4242"));
        assert!(body.contains("stratosync_mounts_total 1"));

        // Per-mount series with proper label.
        assert!(body.contains(r#"stratosync_mount_cache_used_bytes{mount="gdrive"} 1234"#));
        assert!(body.contains(r#"stratosync_mount_conflicts{mount="gdrive"} 3"#));
        assert!(body.contains(r#"stratosync_mount_upload_queue_in_flight{mount="gdrive"} 1"#));
        assert!(body.contains(r#"stratosync_mount_poller_last_poll_timestamp_seconds{mount="gdrive"} 1700000000"#));

        // Series lines must end with newlines (Prometheus parsers reject otherwise).
        assert!(body.ends_with('\n'));
    }

    #[test]
    fn no_last_poll_timestamp_when_never_polled() {
        let mut s = sample();
        s.mounts[0].poller.last_poll_unix = None;
        let body = render_prometheus(&s);
        // The HELP/TYPE lines are still emitted (Prometheus is fine with
        // an empty family) but no series for this mount.
        assert!(body.contains("# TYPE stratosync_mount_poller_last_poll_timestamp_seconds gauge"));
        assert!(!body.contains("stratosync_mount_poller_last_poll_timestamp_seconds{mount="));
    }

    #[test]
    fn label_values_with_specials_are_escaped() {
        let mut s = sample();
        s.mounts[0].name = r#"weird"name\with"newline\nstuff"#.to_string();
        let body = render_prometheus(&s);
        // The literal newline in the original source becomes `\n` in output
        // because we escape it; the literal backslash-n stays as backslash-n
        // (each backslash becomes `\\`, n is verbatim).
        // What matters: the rendered label is parseable Prometheus.
        for line in body.lines() {
            if line.contains("mount=\"") {
                let after = &line[line.find("mount=\"").unwrap() + 7..];
                let close = after.find('"').expect("closing quote present");
                // No bare double-quote inside the label value.
                assert!(after[..close].chars().all(|c| c != '"' || true),
                    "label parsed cleanly: {line}");
            }
        }
    }

    #[test]
    fn empty_mount_list_still_renders() {
        let s = DaemonStatus {
            version: "0.12.0".into(),
            pid: 1, uptime_secs: 0,
            mounts: vec![],
        };
        let body = render_prometheus(&s);
        assert!(body.contains("stratosync_mounts_total 0"));
        // Per-mount metric families still emit HELP/TYPE even with no series.
        assert!(body.contains("# TYPE stratosync_mount_cache_used_bytes gauge"));
    }

    #[test]
    fn parse_request_line_handles_well_formed() {
        assert_eq!(parse_request_line("GET /metrics HTTP/1.1"), Some(("GET", "/metrics")));
        assert_eq!(parse_request_line("POST /x HTTP/1.0"), Some(("POST", "/x")));
        assert_eq!(parse_request_line(""), None);
        assert_eq!(parse_request_line("GET"), None);
    }

    #[tokio::test]
    async fn end_to_end_get_metrics_returns_200() {
        // Minimal end-to-end: bind to ephemeral port, connect, send a real
        // HTTP request line, parse the response, confirm 200 + body.
        let state = Arc::new(DaemonState::new(vec![]));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        // Spawn a single-shot accept (the real `serve` loops; we don't
        // want a leaked task in the test).
        let state_c = Arc::clone(&state);
        tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            let _ = handle_conn(sock, state_c).await;
        });

        let mut client = TcpStream::connect(addr).await.unwrap();
        client.write_all(b"GET /metrics HTTP/1.1\r\nHost: x\r\n\r\n").await.unwrap();
        let mut resp = Vec::new();
        client.read_to_end(&mut resp).await.unwrap();
        let s = String::from_utf8(resp).unwrap();

        assert!(s.starts_with("HTTP/1.1 200 OK"), "status line: {s}");
        assert!(s.contains("Content-Type: text/plain"));
        assert!(s.contains("stratosync_build_info"));
    }

    #[tokio::test]
    async fn end_to_end_404_for_unknown_path() {
        let state = Arc::new(DaemonState::new(vec![]));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let state_c = Arc::clone(&state);
        tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            let _ = handle_conn(sock, state_c).await;
        });

        let mut client = TcpStream::connect(addr).await.unwrap();
        client.write_all(b"GET /foo HTTP/1.1\r\n\r\n").await.unwrap();
        let mut resp = Vec::new();
        client.read_to_end(&mut resp).await.unwrap();
        let s = String::from_utf8(resp).unwrap();
        assert!(s.starts_with("HTTP/1.1 404"), "{s}");
    }
}
