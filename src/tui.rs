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
use base64::Engine as _;
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::style::Print;
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

/// Top-level input mode. When in `SearchInput`, the footer turns into a
/// `/regex_` prompt and most normal keybindings are suppressed so users
/// can type search queries freely. `Normal` is the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Normal,
    SearchInput,
}

/// Active search state. `input` is the text the user is composing (or
/// has committed); `regex` is the compiled form, `None` when the input
/// is empty or fails to parse. Matches are computed on demand during
/// n/N navigation — the log buffer isn't large enough (5000 lines max)
/// for the per-frame cost to matter for highlighting.
struct Search {
    input: String,
    regex: Option<regex::Regex>,
    error: Option<String>,
}

impl Search {
    fn new() -> Self {
        Self {
            input: String::new(),
            regex: None,
            error: None,
        }
    }

    fn recompile(&mut self) {
        if self.input.is_empty() {
            self.regex = None;
            self.error = None;
            return;
        }
        match regex::RegexBuilder::new(&self.input)
            .case_insensitive(!self.input.chars().any(|c| c.is_uppercase()))
            .build()
        {
            Ok(re) => {
                self.regex = Some(re);
                self.error = None;
            }
            Err(e) => {
                self.regex = None;
                self.error = Some(e.to_string());
            }
        }
    }
}

struct LogLine {
    /// Plain text without ANSI sequences. Search operates on this form;
    /// yank uses it so clipboard content is plain text even when the log
    /// line has color spans. Parsing once at ingest keeps render cheap.
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
    /// Inner height of the log pane on the last render. Used by the yank
    /// handler to know which slice of the buffer was actually visible.
    /// 0 before the first draw.
    log_viewport_height: usize,
    mode: Mode,
    search: Search,
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
            log_viewport_height: 0,
            mode: Mode::Normal,
            search: Search::new(),
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

    /// Plain-text slice of the log buffer currently on screen. Mirrors the
    /// windowing done in `draw_log_pane` so yanks reflect exactly what the
    /// user sees. Returns an empty slice if the pane hasn't been drawn yet.
    fn visible_log_lines(&self) -> impl Iterator<Item = &str> {
        let total = self.logs.len();
        let end = total.saturating_sub(self.log_scrollback);
        let start = end.saturating_sub(self.log_viewport_height);
        self.logs
            .iter()
            .skip(start)
            .take(end - start)
            .map(|l| l.plain.as_str())
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

/// Render a single plain-text log line with regex matches highlighted.
/// Non-matching runs become unstyled spans; matches get a yellow
/// background. All find-iter ranges are byte offsets into the plain
/// string, so we slice with standard string indexing.
fn highlight_line(plain: &str, re: &regex::Regex) -> Line<'static> {
    let matches: Vec<(usize, usize)> = re.find_iter(plain).map(|m| (m.start(), m.end())).collect();
    if matches.is_empty() {
        return Line::from(plain.to_string());
    }
    let hl = Style::default().bg(Color::Yellow).fg(Color::Black);
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(matches.len() * 2 + 1);
    let mut pos = 0;
    for (s, e) in matches {
        if s > pos {
            spans.push(Span::raw(plain[pos..s].to_string()));
        }
        spans.push(Span::styled(plain[s..e].to_string(), hl));
        pos = e;
    }
    if pos < plain.len() {
        spans.push(Span::raw(plain[pos..].to_string()));
    }
    Line::from(spans)
}

/// Build an OSC 52 clipboard-set escape for `text`. When running inside
/// tmux, wrap the inner sequence so tmux forwards it to the outer
/// terminal rather than swallowing it. This works over plain SSH since
/// the terminal emulator on the user's end parses OSC 52 regardless of
/// how many hops the bytes travelled.
fn osc52_sequence(text: &str) -> String {
    let b64 = base64::engine::general_purpose::STANDARD.encode(text);
    let inner = format!("\x1b]52;c;{b64}\x07");
    if std::env::var_os("TMUX").is_some() {
        // tmux passthrough: DCS ... ST with a literal ESC before the OSC.
        format!("\x1bPtmux;\x1b{inner}\x1b\\")
    } else {
        inner
    }
}

/// Emit an OSC 52 clipboard-set escape on the terminal. Errors are
/// swallowed — clipboard failures shouldn't crash the TUI; the caller
/// still sets a status-bar message, so the user sees whether the yank
/// "took".
fn copy_to_clipboard(text: &str) -> bool {
    let seq = osc52_sequence(text);
    execute!(stdout(), Print(seq)).is_ok()
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

    preload_log_tail(&mut app).await;

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

/// Max bytes to read from the tail of the log file at startup. 256 KiB is
/// plenty for ~500 colourful lines — more than enough context without
/// slowing the first render on a long-lived daemon whose log ran to MB.
const PRELOAD_TAIL_BYTES: u64 = 256 * 1024;

/// Max lines from the preload window to actually push into the buffer.
/// Well under BUFFER_CAP so users still have headroom for live tail.
const PRELOAD_LINES: usize = 500;

/// Load recent lines from the end of the daemon log so the TUI opens with
/// context instead of an empty pane. Skips a partial first line when the
/// tail window starts mid-line. Advances `log_offset` to end-of-file so
/// the regular poll loop picks up from exactly where we stopped.
async fn preload_log_tail(app: &mut App) {
    let path = app.paths.daemon_log.clone();
    let Ok(meta) = tokio::fs::metadata(&path).await else {
        return;
    };
    let len = meta.len();
    let start = len.saturating_sub(PRELOAD_TAIL_BYTES);
    let Ok(mut file) = File::open(&path).await else {
        app.log_offset = len;
        return;
    };
    if file.seek(std::io::SeekFrom::Start(start)).await.is_err() {
        app.log_offset = len;
        return;
    }
    let mut buf = Vec::with_capacity((len - start) as usize);
    if file.read_to_end(&mut buf).await.is_err() {
        app.log_offset = len;
        return;
    }
    let text = String::from_utf8_lossy(&buf);
    let mut lines: Vec<&str> = text.lines().collect();
    // Drop the leading partial line if we started mid-file.
    if start > 0 && !lines.is_empty() {
        lines.remove(0);
    }
    let skip = lines.len().saturating_sub(PRELOAD_LINES);
    for line in lines.iter().skip(skip) {
        if !line.is_empty() {
            app.push_log_line(line);
        }
    }
    app.log_offset = len;
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
    if app.mode == Mode::SearchInput {
        handle_search_input(app, code, mods);
        return;
    }
    match (code, mods) {
        (KeyCode::Char('q'), _) | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
            app.should_quit = true;
        }
        (KeyCode::Char('Q'), _) => {
            // Shift-Q: stop everything and quit, mirroring `decompose down`.
            // Lower-case q detaches without touching services.
            match send_request(
                &app.paths,
                Request::Down {
                    timeout_seconds: None,
                },
            )
            .await
            {
                Ok(_) => app.set_status("stopping services…"),
                Err(e) => app.set_status(format!("down failed: {e}")),
            }
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
        (KeyCode::Char('y'), _) if app.focus == Focus::Logs => {
            let lines: Vec<&str> = app.visible_log_lines().collect();
            if lines.is_empty() {
                app.set_status("nothing to yank");
            } else {
                let n = lines.len();
                let text = lines.join("\n");
                let ok = copy_to_clipboard(&text);
                app.set_status(if ok {
                    format!("yanked {n} visible line{}", if n == 1 { "" } else { "s" })
                } else {
                    "clipboard copy failed".to_string()
                });
            }
        }
        (KeyCode::Char('Y'), _) if app.focus == Focus::Logs => {
            if app.logs.is_empty() {
                app.set_status("nothing to yank");
            } else {
                let n = app.logs.len();
                let text = app
                    .logs
                    .iter()
                    .map(|l| l.plain.as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                let ok = copy_to_clipboard(&text);
                app.set_status(if ok {
                    format!("yanked {n} lines (full buffer)")
                } else {
                    "clipboard copy failed".to_string()
                });
            }
        }
        (KeyCode::Char('/'), _) => {
            app.mode = Mode::SearchInput;
            app.search.input.clear();
            app.search.recompile();
        }
        (KeyCode::Esc, _) if app.search.regex.is_some() || !app.search.input.is_empty() => {
            // Clear a committed search when not in input mode.
            app.search = Search::new();
        }
        (KeyCode::Char('n'), _) if app.search.regex.is_some() => {
            jump_to_match(app, 1);
        }
        (KeyCode::Char('N'), _) if app.search.regex.is_some() => {
            jump_to_match(app, -1);
        }
        (KeyCode::Char('s'), _) => send_service_action(app, ServiceAction::Stop).await,
        (KeyCode::Char('r'), _) => send_service_action(app, ServiceAction::Restart).await,
        (KeyCode::Char('u'), _) => send_service_action(app, ServiceAction::Start).await,
        _ => {}
    }
}

fn handle_search_input(app: &mut App, code: KeyCode, _mods: KeyModifiers) {
    match code {
        KeyCode::Esc => {
            app.search = Search::new();
            app.mode = Mode::Normal;
        }
        KeyCode::Enter => {
            app.mode = Mode::Normal;
            if app.search.regex.is_some() {
                jump_to_match(app, 1);
            }
        }
        KeyCode::Backspace => {
            app.search.input.pop();
            app.search.recompile();
        }
        KeyCode::Char(c) => {
            app.search.input.push(c);
            app.search.recompile();
        }
        _ => {}
    }
}

/// Jump `direction` (+1 = forward/towards newer, -1 = backward) to the
/// next line in the buffer that matches the current regex. On hit,
/// pause follow and position the match so it's visible. Wraps around
/// at either end.
fn jump_to_match(app: &mut App, direction: i32) {
    let Some(re) = app.search.regex.clone() else {
        return;
    };
    if app.logs.is_empty() {
        return;
    }
    let total = app.logs.len();
    // Current "cursor" line = the bottom-most visible line when paused,
    // or end-of-buffer when following. Map both back to a buffer index.
    let visible_end = total.saturating_sub(app.log_scrollback);
    let cursor = visible_end.saturating_sub(1);
    let next = if direction > 0 {
        // Search forward from (cursor+1), wrapping to 0.
        (cursor + 1..total)
            .chain(0..=cursor)
            .find(|&i| re.is_match(&app.logs[i].plain))
    } else {
        // Search backward from (cursor-1), wrapping to (total-1).
        (0..cursor)
            .rev()
            .chain((cursor..total).rev())
            .find(|&i| re.is_match(&app.logs[i].plain))
    };
    match next {
        Some(i) => {
            app.follow = false;
            // Position the matching line at the bottom of the viewport.
            app.log_scrollback = total.saturating_sub(i + 1);
            app.set_status(format!("match on line {} of {total}", i + 1));
        }
        None => app.set_status(format!("no match for /{}/", app.search.input)),
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
    let bold = Style::default().add_modifier(Modifier::BOLD);
    // Leading two spaces match the List highlight gutter ("▸ ") so header
    // columns line up with the rows beneath.
    let header = Line::from(vec![
        Span::raw("  "),
        Span::styled(format!("{:<12}", "STATE"), bold),
        Span::styled(format!("{:<22}", "NAME"), bold),
        Span::styled(format!("{:>8}", "PID"), bold),
        Span::styled(format!("{:>8}", "RESTART"), bold),
    ]);

    let items: Vec<ListItem> = app
        .processes
        .iter()
        .map(|p| {
            let (glyph, label, _astyle) =
                crate::output::unified_state(&p.state, p.has_readiness_probe, p.ready, false);
            let state_color = state_color(&p.state, p.has_readiness_probe, p.ready);
            let pid = p
                .pid
                .map(|x| x.to_string())
                .unwrap_or_else(|| "-".to_string());
            // For exited processes the state column carries the exit code
            // so users see "exited 0" vs. "exited 1" at a glance.
            let state_text = match (p.state.as_str(), p.exit_code) {
                ("exited", Some(c)) | ("failed", Some(c)) => format!("{label} {c}"),
                _ => label.to_string(),
            };
            let restarts = if p.restart_count > 0 {
                p.restart_count.to_string()
            } else {
                "-".to_string()
            };
            let row = Line::from(vec![
                Span::styled(format!("{glyph} "), Style::default().fg(state_color)),
                Span::styled(
                    format!("{:<10}", truncate(&state_text, 10)),
                    Style::default().fg(state_color),
                ),
                Span::raw(format!("{:<22}", truncate(&p.name, 22))),
                Span::raw(format!("{:>8}", pid)),
                Span::raw(format!("{:>8}", restarts)),
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

fn draw_log_pane(f: &mut ratatui::Frame, area: Rect, app: &mut App) {
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
    app.log_viewport_height = height;
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
        .map(|l| match &app.search.regex {
            // When a search is active, re-render from plain text with match
            // highlights. Loses ANSI coloring in the log itself during
            // search — worth it for visible highlights, and rare enough
            // that trading off once is the right call.
            Some(re) => highlight_line(&l.plain, re),
            None => l.styled.clone(),
        })
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_footer(f: &mut ratatui::Frame, area: Rect, app: &App) {
    // Input mode wins over everything else — show the search prompt with
    // a block cursor so users see what they're typing.
    if app.mode == Mode::SearchInput {
        let mut spans = vec![
            Span::styled("/", Style::default().fg(Color::Yellow)),
            Span::raw(app.search.input.clone()),
            Span::styled("_", Style::default().add_modifier(Modifier::SLOW_BLINK)),
        ];
        if let Some(err) = &app.search.error {
            spans.push(Span::raw("  "));
            spans.push(Span::styled(
                format!("invalid regex: {err}"),
                Style::default().fg(Color::Red),
            ));
        }
        f.render_widget(Paragraph::new(Line::from(spans)), area);
        return;
    }
    let now = Instant::now();
    let text = if let Some((t, msg)) = &app.status_message {
        if now.duration_since(*t) < Duration::from_secs(3) {
            msg.clone()
        } else {
            default_help(app)
        }
    } else {
        default_help(app)
    };
    let mut spans = Vec::new();
    if let Some(re) = &app.search.regex {
        spans.push(Span::styled(
            format!("[/{}/ ] ", re.as_str()),
            Style::default().fg(Color::Yellow),
        ));
    }
    spans.push(Span::styled(text, Style::default().fg(Color::DarkGray)));
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn default_help(app: &App) -> String {
    let search_hint = if app.search.regex.is_some() {
        " · n/N navigate · Esc clear"
    } else {
        " · / search"
    };
    match app.focus {
        Focus::List => format!(
            "↑↓ select · s stop · r restart · u start · tab logs{search_hint} · q detach · Q down"
        ),
        Focus::Logs => format!(
            "PgUp/PgDn scroll · p pause · y yank · Y yank all · End follow{search_hint} · q detach"
        ),
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

/// Color for the STATE column. Matches the palette used by the CLI `ps`
/// table so users see the same thing in both views.
fn state_color(state: &str, has_readiness_probe: bool, healthy: bool) -> Color {
    match state {
        "running" if !has_readiness_probe || healthy => Color::Green,
        "running" | "pending" | "restarting" => Color::Yellow,
        "failed" | "failed_to_start" => Color::Red,
        "exited" | "stopped" | "disabled" | "not_started" => Color::DarkGray,
        _ => Color::White,
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

    #[test]
    fn highlight_line_splits_on_matches() {
        // Regression: the match ranges returned by regex::find_iter are
        // byte offsets. Slicing the original &str at those byte offsets
        // must land on char boundaries — test with ASCII where match bytes
        // == char offsets.
        let re = regex::Regex::new("err").unwrap();
        let line = highlight_line("an error line", &re);
        let texts: Vec<String> = line.spans.iter().map(|s| s.content.to_string()).collect();
        assert_eq!(texts, vec!["an ", "err", "or line"]);
        // Highlighted span carries the yellow background.
        assert_eq!(line.spans[1].style.bg, Some(Color::Yellow));
        // Surrounding spans are unstyled.
        assert_eq!(line.spans[0].style.bg, None);
        assert_eq!(line.spans[2].style.bg, None);
    }

    #[test]
    fn highlight_line_with_no_match_returns_single_span() {
        let re = regex::Regex::new("nope").unwrap();
        let line = highlight_line("an error line", &re);
        assert_eq!(line.spans.len(), 1);
        assert_eq!(line.spans[0].content.as_ref(), "an error line");
    }

    #[test]
    fn search_recompile_case_smartcase() {
        // Lowercase query → case-insensitive (so /error matches "ERROR").
        let mut s = Search::new();
        s.input = "error".into();
        s.recompile();
        assert!(s.regex.as_ref().unwrap().is_match("ERROR boom"));
        // Uppercase anywhere → case-sensitive.
        s.input = "Error".into();
        s.recompile();
        assert!(!s.regex.as_ref().unwrap().is_match("error boom"));
    }

    #[test]
    fn osc52_sequence_framing_plain_and_tmux() {
        // The OSC 52 wire format is ESC ] 52 ; c ; <base64> BEL. Under tmux
        // we wrap the inner sequence in a DCS tmux; ... ST envelope so tmux
        // forwards it to the outer terminal instead of swallowing it.
        // Both branches tested in one test because they share the TMUX env
        // var — running them in parallel would race.
        unsafe { std::env::remove_var("TMUX") };
        let plain = osc52_sequence("hello");
        assert!(
            plain.starts_with("\x1b]52;c;"),
            "missing OSC 52 prefix: {plain:?}"
        );
        assert!(plain.ends_with('\x07'), "missing BEL terminator: {plain:?}");
        // "hello" in base64 is "aGVsbG8=".
        assert!(plain.contains("aGVsbG8="), "unexpected payload: {plain:?}");

        unsafe { std::env::set_var("TMUX", "/tmp/fake,1,0") };
        let wrapped = osc52_sequence("hi");
        unsafe { std::env::remove_var("TMUX") };
        assert!(
            wrapped.starts_with("\x1bPtmux;\x1b"),
            "missing DCS: {wrapped:?}"
        );
        assert!(wrapped.ends_with("\x1b\\"), "missing ST: {wrapped:?}");
        assert!(
            wrapped.contains("\x1b]52;c;"),
            "missing inner OSC: {wrapped:?}"
        );
    }

    #[test]
    fn search_recompile_reports_invalid_regex() {
        // An unbalanced paren must surface as a user-visible error instead
        // of silently dropping the query — otherwise / reports "no match"
        // and users can't tell the difference between "no hits" and "my
        // regex is broken".
        let mut s = Search::new();
        s.input = "(unclosed".into();
        s.recompile();
        assert!(s.regex.is_none());
        assert!(s.error.is_some(), "expected error message for bad regex");
    }

    #[test]
    fn search_recompile_empty_input_clears_state() {
        let mut s = Search::new();
        s.input = "err".into();
        s.recompile();
        assert!(s.regex.is_some());
        s.input.clear();
        s.recompile();
        assert!(s.regex.is_none());
        assert!(s.error.is_none());
    }

    #[test]
    fn highlight_line_handles_multibyte_utf8() {
        // Regression: find_iter returns byte offsets. If we ever sliced at
        // char boundaries instead, a match after an emoji would panic.
        let re = regex::Regex::new("err").unwrap();
        let line = highlight_line("🔥 error here", &re);
        let joined: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(joined, "🔥 error here");
        let hit = line
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "err")
            .expect("match span present");
        assert_eq!(hit.style.bg, Some(Color::Yellow));
    }

    #[test]
    fn truncate_preserves_short_and_ellipsizes_long() {
        assert_eq!(truncate("abc", 5), "abc");
        assert_eq!(truncate("abcdef", 5), "abcd…");
        // Counts chars, not bytes — multi-byte must not blow up.
        assert_eq!(truncate("é".repeat(3).as_str(), 5), "ééé");
    }

    #[test]
    fn process_list_height_grows_caps_and_floors() {
        // Small N with tall screen: 5 rows min (borders + header + a row).
        assert_eq!(process_list_height(0, 40), 5);
        assert_eq!(process_list_height(1, 40), 5);
        // Growth: N=5 needs 5+3=8.
        assert_eq!(process_list_height(5, 40), 8);
        // Cap at half-screen so the log pane keeps its share.
        assert_eq!(process_list_height(100, 40), 20);
        // Tiny screen — floor at 5 wins over the half-cap.
        assert_eq!(process_list_height(10, 6), 5);
    }

    #[test]
    fn state_color_matches_cli_palette() {
        // Running with no probe or passing probe: green.
        assert_eq!(state_color("running", false, false), Color::Green);
        assert_eq!(state_color("running", true, true), Color::Green);
        // Running with failing probe: yellow, so users see "not ready".
        assert_eq!(state_color("running", true, false), Color::Yellow);
        // Pending/restarting: yellow (in-flight).
        assert_eq!(state_color("pending", false, false), Color::Yellow);
        assert_eq!(state_color("restarting", false, false), Color::Yellow);
        // Failure variants: red.
        assert_eq!(state_color("failed", false, false), Color::Red);
        assert_eq!(state_color("failed_to_start", false, false), Color::Red);
        // Neutral terminal states: dim.
        assert_eq!(state_color("exited", false, false), Color::DarkGray);
        assert_eq!(state_color("stopped", false, false), Color::DarkGray);
        // Unknown state falls back to white so nothing is silently hidden.
        assert_eq!(state_color("martian", false, false), Color::White);
    }

    #[test]
    fn default_help_tracks_focus_and_search_state() {
        let mut app = App::new(sample_paths());
        // List focus, no search: mentions process actions and "/ search".
        let list_help = default_help(&app);
        assert!(list_help.contains("stop"), "list help: {list_help}");
        assert!(list_help.contains("/ search"), "list help: {list_help}");
        // Logs focus surfaces yank/scroll hints.
        app.focus = Focus::Logs;
        let logs_help = default_help(&app);
        assert!(logs_help.contains("yank"), "logs help: {logs_help}");
        assert!(logs_help.contains("pause"), "logs help: {logs_help}");
        // Active search swaps the search hint to n/N navigation.
        app.search.input = "e".into();
        app.search.recompile();
        let searching = default_help(&app);
        assert!(searching.contains("n/N"), "search help: {searching}");
        assert!(!searching.contains("/ search"), "search help: {searching}");
    }

    #[test]
    fn jump_to_match_navigates_and_wraps() {
        let mut app = App::new(sample_paths());
        for line in ["alpha", "bravo error", "charlie", "delta error", "echo"] {
            app.push_log_line(line);
        }
        app.search.input = "error".into();
        app.search.recompile();

        // Forward from top lands on the first match (idx 1 → scrollback = 3).
        jump_to_match(&mut app, 1);
        assert_eq!(app.log_scrollback, 3, "first forward hit at idx 1");
        assert!(!app.follow, "jumping must pause follow");

        // Another forward: idx 3 → scrollback = 1.
        jump_to_match(&mut app, 1);
        assert_eq!(app.log_scrollback, 1);

        // Wrap: forward from last match returns to the first.
        jump_to_match(&mut app, 1);
        assert_eq!(app.log_scrollback, 3);

        // Backward from the first match wraps to the last.
        jump_to_match(&mut app, -1);
        assert_eq!(app.log_scrollback, 1);
    }

    #[test]
    fn move_selection_clamps_and_no_ops_when_empty() {
        let mut app = App::new(sample_paths());
        // Empty process list: must not panic, selection stays unset.
        move_selection(&mut app, 1);
        assert_eq!(app.list_state.selected(), None);

        app.processes = vec![
            sample_snapshot("a"),
            sample_snapshot("b"),
            sample_snapshot("c"),
        ];
        app.list_state.select(Some(0));
        move_selection(&mut app, 10); // clamps at len-1
        assert_eq!(app.list_state.selected(), Some(2));
        move_selection(&mut app, -99); // clamps at 0
        assert_eq!(app.list_state.selected(), Some(0));
    }

    fn sample_paths() -> RuntimePaths {
        RuntimePaths {
            socket: "/tmp/decompose-test.sock".into(),
            pid: "/tmp/decompose-test.pid".into(),
            daemon_log: "/tmp/decompose-test.log".into(),
            lock: "/tmp/decompose-test.lock".into(),
        }
    }

    fn sample_snapshot(name: &str) -> ProcessSnapshot {
        ProcessSnapshot {
            name: name.to_string(),
            base: name.to_string(),
            replica: 0,
            status: "running".to_string(),
            state: "running".to_string(),
            description: None,
            restart_count: 0,
            log_ready: false,
            ready: false,
            alive: true,
            has_readiness_probe: false,
            has_liveness_probe: false,
            pid: Some(1),
            exit_code: None,
        }
    }
}
