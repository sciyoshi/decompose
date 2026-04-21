//! Preliminary TUI for `decompose up --tui`.
//!
//! Two-pane layout: process list on top, interleaved log stream on bottom.
//! Polls the daemon for process snapshots via IPC and tails the daemon log
//! file directly (same file `decompose logs` reads). Mouse capture is
//! intentionally off so native terminal drag-select still works.

use std::collections::VecDeque;
use std::io::{Stdout, stdout};
use std::time::Duration;

use ansi_to_tui::IntoText;
use anyhow::{Context, Result};
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::time::{Instant, interval};

use crate::ipc::{Request, Response, send_request};
use crate::model::{ProcessSnapshot, RuntimePaths};

/// Max log lines retained in memory. A chatty service at ~100 lines/sec fills
/// this in under a minute — the on-disk log file is authoritative, the buffer
/// is just what the TUI renders. Tune by `TUI_BUFFER_CAP`.
const BUFFER_CAP: usize = 5_000;

/// How often to poll the daemon for a fresh process list. Balances IPC cost
/// against UI staleness; most status changes (start/stop/restart) also
/// trigger user input that will refresh anyway.
const PS_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// How often to poll the log file for new bytes. Matches
/// `stream_daemon_logs` in `lib.rs` for consistency.
const LOG_POLL_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    List,
    Logs,
}

struct LogLine {
    /// Plain text without ANSI sequences. Search and yank (follow-ups) will
    /// operate on this form; keeping it here so ingest parses once.
    #[allow(dead_code)]
    plain: String,
    /// ANSI parsed into styled spans once at ingest. Rendering a 10k-line
    /// buffer at 60fps means we can't reparse per frame.
    styled: Line<'static>,
}

struct App {
    paths: RuntimePaths,
    processes: Vec<ProcessSnapshot>,
    list_state: ListState,
    focus: Focus,
    logs: VecDeque<LogLine>,
    log_offset: u64,
    /// When true, the log view snaps to the bottom on each new line. Flipped
    /// off when the user scrolls up, back on when they hit End.
    follow: bool,
    /// Rows of scroll applied when `!follow`. Counts from the bottom of the
    /// buffer (0 = newest line at the bottom of the pane).
    log_scrollback: usize,
    status_message: Option<(Instant, String)>,
    should_quit: bool,
}

impl App {
    fn new(paths: RuntimePaths) -> Self {
        Self {
            paths,
            processes: Vec::new(),
            list_state: ListState::default(),
            focus: Focus::List,
            logs: VecDeque::with_capacity(BUFFER_CAP),
            log_offset: 0,
            follow: true,
            log_scrollback: 0,
            status_message: None,
            should_quit: false,
        }
    }

    fn selected_service(&self) -> Option<&ProcessSnapshot> {
        self.list_state
            .selected()
            .and_then(|i| self.processes.get(i))
    }

    fn push_log_line(&mut self, raw: &str) {
        let plain = strip_ansi(raw);
        let styled = parse_ansi_line(raw, &plain);
        if self.logs.len() >= BUFFER_CAP {
            self.logs.pop_front();
        }
        self.logs.push_back(LogLine { plain, styled });
    }

    fn set_status(&mut self, msg: impl Into<String>) {
        self.status_message = Some((Instant::now(), msg.into()));
    }
}

/// Parse a single log line's ANSI SGR sequences into a `Line` of styled
/// spans. Falls back to a plain `Line` if the byte sequence isn't valid
/// ANSI. ansi-to-tui returns a `Text` that may contain multiple `Line`s
/// (if the input embeds `\n`); we flatten to one `Line` since the daemon
/// log is already split line-by-line upstream.
fn parse_ansi_line(raw: &str, plain: &str) -> Line<'static> {
    match raw.as_bytes().into_text() {
        Ok(text) => {
            let mut spans: Vec<Span<'static>> = Vec::new();
            for line in text.lines {
                for span in line.spans {
                    spans.push(span);
                }
            }
            if spans.is_empty() {
                Line::from(plain.to_string())
            } else {
                Line::from(spans)
            }
        }
        Err(_) => Line::from(plain.to_string()),
    }
}

/// Lossy ANSI strip — enough for search/yank without pulling in a full
/// terminal emulator.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut bytes = s.bytes();
    while let Some(b) = bytes.next() {
        if b == 0x1b {
            // Skip ESC then a CSI-style sequence up to a final byte in @–~.
            if let Some(next) = bytes.next() {
                if next == b'[' {
                    for c in bytes.by_ref() {
                        if (0x40..=0x7e).contains(&c) {
                            break;
                        }
                    }
                } else if (0x40..=0x5f).contains(&next) {
                    // Two-byte escape: already consumed.
                }
            }
        } else {
            out.push(b as char);
        }
    }
    out
}

pub async fn run(paths: RuntimePaths) -> Result<()> {
    let mut terminal = setup_terminal().context("failed to initialise terminal for TUI")?;
    let result = run_app(&mut terminal, paths).await;
    restore_terminal(&mut terminal).ok();
    result
}

type Term = Terminal<CrosstermBackend<Stdout>>;

fn setup_terminal() -> Result<Term> {
    enable_raw_mode()?;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen)?;
    Ok(Terminal::new(CrosstermBackend::new(out))?)
}

fn restore_terminal(term: &mut Term) -> Result<()> {
    disable_raw_mode()?;
    execute!(term.backend_mut(), LeaveAlternateScreen)?;
    term.show_cursor()?;
    Ok(())
}

async fn run_app(term: &mut Term, paths: RuntimePaths) -> Result<()> {
    let mut app = App::new(paths);

    // Start tailing the log file at its current end so we don't flood the
    // buffer with historical output on open. A later improvement: preload
    // the last N lines for context.
    if let Ok(meta) = tokio::fs::metadata(&app.paths.daemon_log).await {
        app.log_offset = meta.len();
    }

    // Prime the process list so the first render isn't empty.
    refresh_processes(&mut app).await;

    let mut events = EventStream::new();
    let mut ps_tick = interval(PS_POLL_INTERVAL);
    ps_tick.tick().await; // consume the immediate first tick
    let mut log_tick = interval(LOG_POLL_INTERVAL);

    term.draw(|f| draw(f, &mut app))?;

    while !app.should_quit {
        tokio::select! {
            maybe_evt = events.next() => {
                match maybe_evt {
                    Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                        handle_key(&mut app, key.code, key.modifiers).await;
                    }
                    Some(Ok(Event::Resize(_, _))) => {}
                    Some(Err(e)) => {
                        app.set_status(format!("input error: {e}"));
                    }
                    None => break,
                    _ => {}
                }
            }
            _ = ps_tick.tick() => {
                refresh_processes(&mut app).await;
            }
            _ = log_tick.tick() => {
                poll_log(&mut app).await;
            }
        }
        term.draw(|f| draw(f, &mut app))?;
    }
    Ok(())
}

async fn refresh_processes(app: &mut App) {
    match send_request(&app.paths, Request::Ps).await {
        Ok(Response::Ps { processes, .. }) => {
            app.processes = processes;
            // Keep the selection in range; default to 0 if nothing selected.
            if app.processes.is_empty() {
                app.list_state.select(None);
            } else {
                let idx = app
                    .list_state
                    .selected()
                    .unwrap_or(0)
                    .min(app.processes.len() - 1);
                app.list_state.select(Some(idx));
            }
        }
        Ok(Response::Error { message }) => {
            app.set_status(format!("daemon error: {message}"));
        }
        Err(e) => {
            app.set_status(format!("daemon unreachable: {e}"));
        }
        _ => {}
    }
}

async fn poll_log(app: &mut App) {
    let path = &app.paths.daemon_log;
    let meta = match tokio::fs::metadata(path).await {
        Ok(m) => m,
        Err(_) => return,
    };
    let len = meta.len();
    if len < app.log_offset {
        // File was truncated (daemon restart): reset to start.
        app.log_offset = 0;
    }
    if len == app.log_offset {
        return;
    }
    let mut file = match File::open(path).await {
        Ok(f) => f,
        Err(_) => return,
    };
    if file
        .seek(std::io::SeekFrom::Start(app.log_offset))
        .await
        .is_err()
    {
        return;
    }
    let mut buf = Vec::with_capacity((len - app.log_offset) as usize);
    if file.read_to_end(&mut buf).await.is_err() {
        return;
    }
    app.log_offset += buf.len() as u64;
    let text = String::from_utf8_lossy(&buf);
    for raw in text.split_inclusive('\n') {
        let line = raw.strip_suffix('\n').unwrap_or(raw);
        if line.is_empty() {
            continue;
        }
        app.push_log_line(line);
    }
    if app.follow {
        app.log_scrollback = 0;
    }
}

async fn handle_key(app: &mut App, code: KeyCode, mods: KeyModifiers) {
    match (code, mods) {
        (KeyCode::Char('q'), _) | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
            app.should_quit = true;
        }
        (KeyCode::Tab, _) => {
            app.focus = match app.focus {
                Focus::List => Focus::Logs,
                Focus::Logs => Focus::List,
            };
        }
        (KeyCode::Char('j'), _) | (KeyCode::Down, _) => move_selection(app, 1),
        (KeyCode::Char('k'), _) | (KeyCode::Up, _) => move_selection(app, -1),
        (KeyCode::Char('g'), _) if !app.processes.is_empty() => {
            app.list_state.select(Some(0));
        }
        (KeyCode::Char('G'), _) if !app.processes.is_empty() => {
            app.list_state.select(Some(app.processes.len() - 1));
        }
        (KeyCode::PageUp, _) => scroll_logs(app, 10),
        (KeyCode::PageDown, _) => scroll_logs(app, -10),
        (KeyCode::Home, _) if app.focus == Focus::Logs => {
            app.follow = false;
            app.log_scrollback = app.logs.len().saturating_sub(1);
        }
        (KeyCode::End, _) if app.focus == Focus::Logs => {
            app.follow = true;
            app.log_scrollback = 0;
        }
        (KeyCode::Char('p'), _) if app.focus == Focus::Logs => {
            app.follow = !app.follow;
            app.set_status(if app.follow { "following" } else { "paused" });
        }
        (KeyCode::Char('s'), _) => send_service_action(app, ServiceAction::Stop).await,
        (KeyCode::Char('r'), _) => send_service_action(app, ServiceAction::Restart).await,
        (KeyCode::Char('u'), _) => send_service_action(app, ServiceAction::Start).await,
        _ => {}
    }
}

fn move_selection(app: &mut App, delta: i32) {
    if app.processes.is_empty() {
        return;
    }
    let len = app.processes.len() as i32;
    let current = app.list_state.selected().unwrap_or(0) as i32;
    let next = (current + delta).clamp(0, len - 1);
    app.list_state.select(Some(next as usize));
}

fn scroll_logs(app: &mut App, delta: i32) {
    if delta > 0 {
        // scroll up (older lines)
        app.follow = false;
        let new = app.log_scrollback.saturating_add(delta as usize);
        app.log_scrollback = new.min(app.logs.len().saturating_sub(1));
    } else {
        let step = (-delta) as usize;
        if app.log_scrollback <= step {
            app.log_scrollback = 0;
            app.follow = true;
        } else {
            app.log_scrollback -= step;
        }
    }
}

#[derive(Copy, Clone)]
enum ServiceAction {
    Stop,
    Start,
    Restart,
}

async fn send_service_action(app: &mut App, action: ServiceAction) {
    let Some(svc) = app.selected_service() else {
        return;
    };
    let target = svc.base.clone();
    let (req, verb) = match action {
        ServiceAction::Stop => (
            Request::Stop {
                services: vec![target.clone()],
            },
            "stop",
        ),
        ServiceAction::Start => (
            Request::Start {
                services: vec![target.clone()],
            },
            "start",
        ),
        ServiceAction::Restart => (
            Request::Restart {
                services: vec![target.clone()],
            },
            "restart",
        ),
    };
    match send_request(&app.paths, req).await {
        Ok(Response::Ack { .. }) => app.set_status(format!("{verb} {target}")),
        Ok(Response::Error { message }) => app.set_status(format!("{verb} failed: {message}")),
        Err(e) => app.set_status(format!("{verb} failed: {e}")),
        _ => {}
    }
    refresh_processes(app).await;
}

fn draw(f: &mut ratatui::Frame, app: &mut App) {
    let size = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(process_list_height(app.processes.len(), size.height)),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(size);

    draw_process_list(f, chunks[0], app);
    draw_log_pane(f, chunks[1], app);
    draw_footer(f, chunks[2], app);
}

fn process_list_height(n: usize, total: u16) -> u16 {
    // +2 for borders, +1 for header row. Cap at half the screen so the log
    // pane always has room; require at least 5 rows.
    let requested = (n as u16).saturating_add(3);
    let cap = (total / 2).max(5);
    requested.clamp(5, cap)
}

fn draw_process_list(f: &mut ratatui::Frame, area: Rect, app: &mut App) {
    let header = Line::from(vec![
        Span::styled(
            format!("{:<18}", "NAME"),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{:<12}", "STATUS"),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{:<8}", "PID"),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled("HEALTH", Style::default().add_modifier(Modifier::BOLD)),
    ]);

    let items: Vec<ListItem> = app
        .processes
        .iter()
        .map(|p| {
            let status_color = status_color(&p.state);
            let pid = p
                .pid
                .map(|x| x.to_string())
                .unwrap_or_else(|| "-".to_string());
            let health = health_glyph(p);
            let row = Line::from(vec![
                Span::raw(format!("{:<18}", truncate(&p.name, 18))),
                Span::styled(
                    format!("{:<12}", p.status),
                    Style::default().fg(status_color),
                ),
                Span::raw(format!("{:<8}", pid)),
                Span::raw(health),
            ]);
            ListItem::new(row)
        })
        .collect();

    let focused = app.focus == Focus::List;
    let title = if focused {
        " processes "
    } else {
        " processes  (tab) "
    };
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if focused {
            Color::Cyan
        } else {
            Color::DarkGray
        }));

    // Render header on the top border, list below.
    let inner = block.inner(area);
    f.render_widget(block, area);
    let list_area = Rect {
        x: inner.x,
        y: inner.y.saturating_add(1),
        width: inner.width,
        height: inner.height.saturating_sub(1),
    };
    let header_area = Rect {
        x: inner.x,
        y: inner.y,
        width: inner.width,
        height: 1,
    };
    f.render_widget(Paragraph::new(header), header_area);

    let list = List::new(items)
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▸ ");
    f.render_stateful_widget(list, list_area, &mut app.list_state);
}

fn draw_log_pane(f: &mut ratatui::Frame, area: Rect, app: &App) {
    let focused = app.focus == Focus::Logs;
    let indicator = if app.follow { "●" } else { "❚❚" };
    let title = format!(
        " logs  {}  {} lines  (tab:switch  p:pause  End:follow) ",
        indicator,
        app.logs.len()
    );
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if focused {
            Color::Cyan
        } else {
            Color::DarkGray
        }));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let height = inner.height as usize;
    if height == 0 {
        return;
    }
    let total = app.logs.len();
    // When following, show the last `height` lines. When paused, show a
    // window that ends `log_scrollback` lines above the newest.
    let end = total.saturating_sub(app.log_scrollback);
    let start = end.saturating_sub(height);
    let lines: Vec<Line<'static>> = app
        .logs
        .iter()
        .skip(start)
        .take(end - start)
        .map(|l| l.styled.clone())
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_footer(f: &mut ratatui::Frame, area: Rect, app: &App) {
    let now = Instant::now();
    let text = if let Some((t, msg)) = &app.status_message {
        if now.duration_since(*t) < Duration::from_secs(3) {
            msg.clone()
        } else {
            default_help(app.focus)
        }
    } else {
        default_help(app.focus)
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            text,
            Style::default().fg(Color::DarkGray),
        ))),
        area,
    );
}

fn default_help(focus: Focus) -> String {
    match focus {
        Focus::List => "↑↓ select · s stop · r restart · u start · tab logs · q quit".to_string(),
        Focus::Logs => "PgUp/PgDn scroll · p pause · End follow · tab list · q quit".to_string(),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

fn status_color(state: &str) -> Color {
    match state {
        "running" => Color::Green,
        "starting" | "pending" => Color::Yellow,
        "exited" => Color::DarkGray,
        "failed" | "stopped" => Color::Red,
        _ => Color::White,
    }
}

fn health_glyph(p: &ProcessSnapshot) -> String {
    let mut parts = Vec::new();
    if p.has_readiness_probe {
        parts.push(if p.ready { "✓ ready" } else { "✗ ready" });
    }
    if p.has_liveness_probe {
        parts.push(if p.alive { "✓ alive" } else { "✗ alive" });
    }
    if parts.is_empty() {
        "-".to_string()
    } else {
        parts.join(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ansi_line_preserves_red_span() {
        // ANSI "31" is red, "0" resets. The parsed line must split the input
        // into at least two spans and the red span must carry a red fg — a
        // regression here means colored daemon log output falls back to a
        // single monochrome span.
        let raw = "\x1b[31merror\x1b[0m tail";
        let line = parse_ansi_line(raw, &strip_ansi(raw));
        assert!(
            line.spans.len() >= 2,
            "expected multiple spans, got {:?}",
            line.spans
        );
        let red = line
            .spans
            .iter()
            .find(|s| s.content.contains("error"))
            .expect("error span present");
        assert_eq!(red.style.fg, Some(Color::Red));
    }

    #[test]
    fn parse_ansi_line_falls_back_for_plain_text() {
        // Plain text has no escape sequences — must come through as a single
        // span equal to the input.
        let raw = "just a line";
        let line = parse_ansi_line(raw, raw);
        let joined: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(joined, "just a line");
    }

    #[test]
    fn strip_ansi_removes_sgr_sequences() {
        assert_eq!(strip_ansi("\x1b[31mred\x1b[0m"), "red");
        assert_eq!(strip_ansi("plain"), "plain");
    }
}
