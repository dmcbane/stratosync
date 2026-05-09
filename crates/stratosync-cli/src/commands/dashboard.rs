//! `stratosync dashboard` — live view of daemon state over the IPC socket.
//!
//! Modes:
//! - interactive (default): ratatui full-screen, refreshes @ 1s, `q` to quit.
//! - `--once`: plain text, render one snapshot and exit.
use std::io;
use std::path::Path;
use std::time::{Duration, Instant, UNIX_EPOCH, SystemTime};

use anyhow::{Context, Result};
use bytesize::ByteSize;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};
use stratosync_core::{
    config::default_runtime_socket,
    ipc::{ActiveHydration, ActiveUpload, DaemonStatus, IpcResponse, MountStatus},
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

pub async fn run(_config_path: &Path, once: bool) -> Result<()> {
    let socket = default_runtime_socket();

    if once {
        let status = fetch_status(&socket).await?;
        print_plain(&status);
        return Ok(());
    }

    run_tui(socket).await
}

// ── IPC client ───────────────────────────────────────────────────────────────

async fn fetch_status(socket: &Path) -> Result<DaemonStatus> {
    let mut stream = UnixStream::connect(socket).await.with_context(|| format!(
        "daemon is not running (no socket at {}); start with stratosyncd",
        socket.display()
    ))?;
    stream.write_all(b"{\"op\":\"status\"}\n").await?;
    stream.shutdown().await?;

    let (rd, _) = stream.into_split();
    let mut reader = BufReader::new(rd);
    let mut line = String::new();
    reader.read_line(&mut line).await?;

    let resp: IpcResponse = serde_json::from_str(line.trim())
        .context("malformed IPC response")?;
    if !resp.ok {
        anyhow::bail!("daemon error: {}", resp.error.unwrap_or_default());
    }
    let data = resp.data.ok_or_else(|| anyhow::anyhow!("empty data"))?;
    Ok(serde_json::from_value(data)?)
}

// ── Plain-text --once output ─────────────────────────────────────────────────

fn print_plain(s: &DaemonStatus) {
    println!("stratosync {} — pid {} — uptime {}",
        s.version, s.pid, fmt_duration(s.uptime_secs));
    println!();
    println!("{:<12} {:<8} {:>18} {:>10} {:>10} {:>10}",
        "mount", "status", "cache", "queue", "hydr", "conflicts");
    println!("{}", "─".repeat(78));
    for m in &s.mounts {
        let pct = pct(m.cache.used_bytes, m.cache.quota_bytes);
        println!("{:<12} {:<8} {:>18} {:>10} {:>10} {:>10}",
            m.name,
            status_label(m),
            format!("{}% ({})", pct, ByteSize(m.cache.used_bytes)),
            format!("{}↑ {}⌛", m.queue.in_flight.len(), m.queue.pending),
            format!("{}/{}", m.hydration.active, m.hydration.waiters),
            m.conflicts,
        );
    }
    for m in &s.mounts {
        if !m.queue.in_flight.is_empty() {
            println!();
            println!("{}: in-flight uploads:", m.name);
            for up in &m.queue.in_flight {
                println!("  {}", fmt_active_upload(up));
            }
        }
        if !m.hydration.in_flight.is_empty() {
            println!();
            println!("{}: in-flight hydrations:", m.name);
            for hy in &m.hydration.in_flight {
                println!("  {}", fmt_active_hydration(hy));
            }
        }
    }
}

// ── Interactive TUI ──────────────────────────────────────────────────────────

async fn run_tui(socket: std::path::PathBuf) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = tui_loop(&mut terminal, &socket).await;

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableMouseCapture)?;
    terminal.show_cursor()?;
    result
}

async fn tui_loop<B: Backend>(
    terminal: &mut Terminal<B>,
    socket: &Path,
) -> Result<()> {
    let mut last_status: Option<DaemonStatus> = None;
    let mut last_error: Option<String> = None;
    let mut selected: usize = 0;
    let mut last_fetch = Instant::now() - Duration::from_secs(10); // fetch immediately
    let refresh = Duration::from_secs(1);

    loop {
        if last_fetch.elapsed() >= refresh {
            match fetch_status(socket).await {
                Ok(s) => { last_status = Some(s); last_error = None; }
                Err(e) => { last_error = Some(format!("{e}")); }
            }
            last_fetch = Instant::now();
        }

        terminal.draw(|frame| render(frame, last_status.as_ref(), last_error.as_deref(), selected))?;

        // Poll for key events with short timeout so we can refresh on schedule.
        if event::poll(Duration::from_millis(200))? {
            if let Event::Key(key) = event::read()? {
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Char('r') => last_fetch = Instant::now() - Duration::from_secs(10),
                    KeyCode::Down | KeyCode::Char('j') => {
                        if let Some(s) = &last_status {
                            if !s.mounts.is_empty() {
                                selected = (selected + 1).min(s.mounts.len() - 1);
                            }
                        }
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        selected = selected.saturating_sub(1);
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

fn render(
    frame: &mut Frame,
    status: Option<&DaemonStatus>,
    error: Option<&str>,
    selected: usize,
) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // header
            Constraint::Min(6),    // mount table
            Constraint::Length(10), // in-flight detail (uploads + hydrations)
            Constraint::Length(7), // poller + hydration detail
            Constraint::Length(1), // help
        ])
        .split(area);

    // Header
    let header_text = match (status, error) {
        (Some(s), _) => format!("stratosync {} — pid {} — uptime {}",
            s.version, s.pid, fmt_duration(s.uptime_secs)),
        (None, Some(e)) => format!("not connected: {e}"),
        (None, None) => "connecting…".to_string(),
    };
    frame.render_widget(
        Paragraph::new(header_text).block(Block::default().borders(Borders::BOTTOM)),
        chunks[0],
    );

    // Mount table
    if let Some(s) = status {
        let header = Row::new(vec!["name", "status", "cache", "queue", "hydr", "conflicts"])
            .style(Style::default().add_modifier(Modifier::BOLD));
        let rows: Vec<Row> = s.mounts.iter().enumerate().map(|(i, m)| {
            let pct = pct(m.cache.used_bytes, m.cache.quota_bytes);
            let cells = vec![
                Cell::from(m.name.clone()),
                Cell::from(status_label(m)),
                Cell::from(format!("{}% ({})", pct, ByteSize(m.cache.used_bytes))),
                Cell::from(format!("{}↑ {}⌛", m.queue.in_flight.len(), m.queue.pending)),
                Cell::from(format!("{}/{}", m.hydration.active, m.hydration.waiters)),
                Cell::from(m.conflicts.to_string()),
            ];
            let row = Row::new(cells);
            if i == selected {
                row.style(Style::default().add_modifier(Modifier::REVERSED))
            } else {
                row
            }
        }).collect();
        let widths = [
            Constraint::Length(14),
            Constraint::Length(8),
            Constraint::Length(22),
            Constraint::Length(14),
            Constraint::Length(10),
            Constraint::Length(10),
        ];
        let table = Table::new(rows, widths)
            .header(header)
            .block(Block::default().title(" mounts ").borders(Borders::ALL));
        frame.render_widget(table, chunks[1]);

        // In-flight detail — uploads on top, hydrations below. Both
        // sections always render so the user can immediately see "(no
        // active downloads)" when their cp appears stuck and confirm
        // there's nothing in flight rather than guessing.
        let mount = s.mounts.get(selected);
        let inflight_text = mount.map(|m| {
            let mut lines = Vec::new();
            lines.push("uploads:".to_string());
            if m.queue.in_flight.is_empty() {
                lines.push("  (none)".to_string());
            } else {
                for up in &m.queue.in_flight {
                    lines.push(format!("  {}", fmt_active_upload(up)));
                }
            }
            lines.push("hydrations:".to_string());
            if m.hydration.in_flight.is_empty() {
                lines.push("  (none)".to_string());
            } else {
                for hy in &m.hydration.in_flight {
                    lines.push(format!("  {}", fmt_active_hydration(hy)));
                }
            }
            lines.join("\n")
        }).unwrap_or_default();
        let title = mount.map(|m| format!(" {}: in-flight ", m.name))
            .unwrap_or_else(|| " in-flight ".to_string());
        frame.render_widget(
            Paragraph::new(inflight_text).block(Block::default().title(title).borders(Borders::ALL)),
            chunks[2],
        );

        // Poller detail (also surfaces hydration health — both are
        // operational signals the user wants in one glance).
        let poller_text = mount.map(|m| {
            let last = m.poller.last_poll_unix.map(|u| format!("{}s ago", elapsed_secs(u)))
                .unwrap_or_else(|| "never".to_string());
            let next = m.poller.next_poll_unix.map(|u| format!("in {}s", until_secs(u)))
                .unwrap_or_else(|| "-".to_string());
            let mut text = format!(
                "mode: {}   last: {}   next: {}\nfailures: {}   interval: {}s{}",
                m.poller.mode, last, next,
                m.poller.consecutive_failures, m.poller.current_interval_secs,
                m.poller.last_error.as_ref().map(|e| format!("\nerror: {e}")).unwrap_or_default(),
            );
            if m.hydration.consecutive_failures > 0 {
                text.push_str(&format!(
                    "\nhydration: {} consecutive fail(s){}",
                    m.hydration.consecutive_failures,
                    m.hydration.last_error.as_ref()
                        .map(|e| format!(" — {e}"))
                        .unwrap_or_default(),
                ));
            }
            text
        }).unwrap_or_default();
        let ptitle = mount.map(|m| format!(" {}: poller ", m.name))
            .unwrap_or_else(|| " poller ".to_string());
        frame.render_widget(
            Paragraph::new(poller_text).block(Block::default().title(ptitle).borders(Borders::ALL)),
            chunks[3],
        );
    }

    // Help line
    frame.render_widget(
        Paragraph::new("[q] quit  [r] refresh  [↑↓/j/k] select mount")
            .style(Style::default().add_modifier(Modifier::DIM)),
        chunks[4],
    );
}

// ── Formatting helpers ───────────────────────────────────────────────────────

fn status_label(m: &MountStatus) -> String {
    // Worst-of poller and hydration. A stalled download is just as bad
    // as a failing poller, and lumping them under one column keeps the
    // overview row scannable.
    let worst = m.poller.consecutive_failures.max(m.hydration.consecutive_failures);
    match worst {
        0    => "● ok".to_string(),
        1..=9 => "◎ retry".to_string(),
        _    => "✕ halt".to_string(),
    }
}

fn fmt_active_upload(up: &ActiveUpload) -> String {
    // Path | progress (uploaded/total) | attempt# | first-seen | this-attempt
    //
    // first_started_unix is 0 on legacy daemons that don't populate it
    // — fall back to started_at_unix so the column reads as "just
    // started" instead of "57 years ago" (epoch).
    let first = if up.first_started_unix == 0 {
        up.started_at_unix
    } else {
        up.first_started_unix
    };
    let cur_elapsed   = elapsed_secs(up.started_at_unix);
    let total_elapsed = elapsed_secs(first);
    let attempt_str = if up.attempt <= 1 {
        String::new()
    } else {
        format!("  attempt#{}", up.attempt)
    };
    let total_str = if total_elapsed > cur_elapsed + 1 {
        // Only show "first-seen" when it's meaningfully different from
        // the current-attempt clock (i.e. we're actually in a retry).
        format!("  first-seen {}", fmt_duration(total_elapsed as u64))
    } else {
        String::new()
    };
    format!("{:<40} {:>11}  {:>4}s{}{}",
        truncate(&up.path, 40),
        fmt_progress(up),
        cur_elapsed,
        attempt_str,
        total_str)
}

/// Render the progress column. Falls back gracefully:
///   - Some(b) and total>0 → `"5.2 MB/100 MB 5%"`
///   - Some(b) but total=0 (rare) → `"5.2 MB"`
///   - None (no progress reporting yet) → `"100 MB"` (just total)
fn fmt_progress(up: &ActiveUpload) -> String {
    fmt_progress_pair(up.bytes_uploaded, up.size_bytes)
}

fn fmt_active_hydration(hy: &ActiveHydration) -> String {
    // Same row shape as fmt_active_upload, with the hydration's
    // first-seen / attempt# semantics (preserved across the FUSE
    // retry-via-recall loop, see HydrationTracker).
    let first = if hy.first_started_unix == 0 {
        hy.started_at_unix
    } else {
        hy.first_started_unix
    };
    let cur_elapsed   = elapsed_secs(hy.started_at_unix);
    let total_elapsed = elapsed_secs(first);
    let attempt_str = if hy.attempt <= 1 {
        String::new()
    } else {
        format!("  attempt#{}", hy.attempt)
    };
    let total_str = if total_elapsed > cur_elapsed + 1 {
        format!("  first-seen {}", fmt_duration(total_elapsed as u64))
    } else {
        String::new()
    };
    format!("{:<40} {:>11}  {:>4}s{}{}",
        truncate(&hy.path, 40),
        fmt_progress_pair(hy.bytes_downloaded, hy.size_bytes),
        cur_elapsed,
        attempt_str,
        total_str)
}

/// Shared progress-column renderer. Same fall-back ladder as
/// `fmt_progress` — kept generic so uploads and hydrations stay
/// visually identical.
fn fmt_progress_pair(bytes: Option<u64>, total_bytes: u64) -> String {
    let total = ByteSize(total_bytes).to_string();
    match bytes {
        Some(b) if total_bytes > 0 => {
            let p = (b as f64 / total_bytes as f64 * 100.0) as u64;
            format!("{}/{} {}%", ByteSize(b), total, p)
        }
        Some(b) => ByteSize(b).to_string(),
        None    => total,
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max { s.to_string() } else { format!("…{}", &s[s.len().saturating_sub(max - 1)..]) }
}

fn pct(used: u64, quota: u64) -> u64 {
    if quota == 0 { 0 } else { (used as f64 / quota as f64 * 100.0) as u64 }
}

fn fmt_duration(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 { format!("{h}h {m}m") }
    else if m > 0 { format!("{m}m {s}s") }
    else { format!("{s}s") }
}

fn now_unix() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn elapsed_secs(past_unix: i64) -> i64 {
    (now_unix() - past_unix).max(0)
}

fn until_secs(future_unix: i64) -> i64 {
    (future_unix - now_unix()).max(0)
}
