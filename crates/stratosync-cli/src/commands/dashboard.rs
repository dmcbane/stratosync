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
    ipc::{ActiveUpload, DaemonStatus, IpcResponse, MountStatus},
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
                println!("  {:<40} {:>10}  {}s",
                    up.path, format!("{}", ByteSize(up.size_bytes)),
                    elapsed_secs(up.started_at_unix));
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
            Constraint::Length(8), // in-flight detail
            Constraint::Length(5), // poller detail
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

        // In-flight detail
        let mount = s.mounts.get(selected);
        let inflight_text = mount.map(|m| {
            if m.queue.in_flight.is_empty() {
                "(no active uploads)".to_string()
            } else {
                m.queue.in_flight.iter()
                    .map(fmt_active_upload)
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        }).unwrap_or_default();
        let title = mount.map(|m| format!(" {}: in-flight ", m.name))
            .unwrap_or_else(|| " in-flight ".to_string());
        frame.render_widget(
            Paragraph::new(inflight_text).block(Block::default().title(title).borders(Borders::ALL)),
            chunks[2],
        );

        // Poller detail
        let poller_text = mount.map(|m| {
            let last = m.poller.last_poll_unix.map(|u| format!("{}s ago", elapsed_secs(u)))
                .unwrap_or_else(|| "never".to_string());
            let next = m.poller.next_poll_unix.map(|u| format!("in {}s", until_secs(u)))
                .unwrap_or_else(|| "-".to_string());
            format!(
                "mode: {}   last: {}   next: {}\nfailures: {}   interval: {}s{}",
                m.poller.mode, last, next,
                m.poller.consecutive_failures, m.poller.current_interval_secs,
                m.poller.last_error.as_ref().map(|e| format!("\nerror: {e}")).unwrap_or_default(),
            )
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
    match m.poller.consecutive_failures {
        0    => "● ok".to_string(),
        1..=9 => "◎ retry".to_string(),
        _    => "✕ halt".to_string(),
    }
}

fn fmt_active_upload(up: &ActiveUpload) -> String {
    format!("{:<40} {:>10}  {}s",
        truncate(&up.path, 40),
        ByteSize(up.size_bytes).to_string(),
        elapsed_secs(up.started_at_unix))
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
