use std::env;
use std::fmt;
use std::io::IsTerminal;

use anstyle::{AnsiColor, Style};
use clap::Args;
use serde::Serialize;

#[derive(Args, Debug, Clone, Default)]
pub struct OutputArgs {
    /// Emit JSON results and diagnostics (JSON Lines for streams). Help, completion,
    /// and child streams retain their native formats; config keeps its own schema.
    #[arg(long, global = true, conflicts_with = "table")]
    pub json: bool,
    /// Emit table/text output.
    #[arg(long, global = true, conflicts_with = "json")]
    pub table: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    Json,
    Table,
}

impl OutputArgs {
    pub fn resolve(&self) -> OutputMode {
        if self.json {
            return OutputMode::Json;
        }
        OutputMode::Table
    }
}

pub fn env_truthy(name: &str) -> bool {
    let Some(raw) = env::var_os(name) else {
        return false;
    };
    let value = raw.to_string_lossy();
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

pub fn print_json<T: Serialize>(value: &T) -> anyhow::Result<()> {
    // Serialize before touching stdout, including the record terminator.
    let mut encoded = serde_json::to_vec(value)?;
    encoded.push(b'\n');
    write_bytes(&encoded)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Color / style helpers
// ---------------------------------------------------------------------------

/// Returns `true` when ANSI colors should be emitted to stdout.
///
/// Disabled when:
/// - `NO_COLOR` env var is set (to any non-empty value)
/// - stdout is not a TTY (and --table was not forced)
pub fn use_color() -> bool {
    if let Some(val) = env::var_os("NO_COLOR")
        && !val.is_empty()
    {
        return false;
    }
    std::io::stdout().is_terminal()
}

/// Resolve a `Style` into the identity style when color is disabled.
fn maybe(style: Style, color: bool) -> Style {
    if color { style } else { Style::new() }
}

const GREEN: Style = Style::new().fg_color(Some(anstyle::Color::Ansi(AnsiColor::Green)));
const YELLOW: Style = Style::new().fg_color(Some(anstyle::Color::Ansi(AnsiColor::Yellow)));
const RED: Style = Style::new().fg_color(Some(anstyle::Color::Ansi(AnsiColor::Red)));
const DIM: Style = Style::new().dimmed();

/// Pick a style for a process status string (the `state` field from `ProcessSnapshot`).
pub fn style_for_status(status: &str, color: bool) -> Style {
    let s = match status {
        "running" => GREEN,
        "exited" => GREEN,
        "healthy" => GREEN,
        "pending" | "initializing" | "starting" | "restarting" => YELLOW,
        "failed" | "failed_to_start" => RED,
        "disabled" | "not_started" | "stopped" => DIM,
        _ => Style::new(),
    };
    maybe(s, color)
}

/// Unified state glyph + label for the STATE column in `ps`.
///
/// Combines process status and health into a single scannable representation:
/// - not_started / stopped / disabled / exited(0): `-` dim
/// - pending / restarting: `◌` yellow
/// - running + healthy (or no probe): `●` green
/// - running + probe failing: `○` yellow
/// - failed / failed_to_start: `✕` red
pub fn unified_state(
    state: &str,
    has_readiness_probe: bool,
    healthy: bool,
    color: bool,
) -> (&'static str, &'static str, Style) {
    let (g, label, s) = match state {
        "running" if !has_readiness_probe || healthy => ("\u{25cf}", "healthy", GREEN), // ●
        "running" => ("\u{25cb}", "running", YELLOW),                                   // ○
        "initializing" => ("\u{25cc}", "initializing", YELLOW),
        "pending" => ("\u{25cc}", "pending", YELLOW), // ◌
        "restarting" => ("\u{25cc}", "restarting", YELLOW), // ◌
        "failed" => ("\u{2715}", "failed", RED),
        "failed_to_start" => ("\u{2715}", "failed_to_start", RED), // ✕
        "exited" => ("-", "exited", DIM),
        "stopped" => ("-", "stopped", DIM),
        "disabled" => ("-", "disabled", DIM),
        "not_started" => ("-", "not_started", DIM),
        _ => ("-", "", Style::new()),
    };
    (g, label, maybe(s, color))
}

/// A small wrapper so we can write colored strings via `format!` / `write!`.
pub struct Styled<'a> {
    pub style: Style,
    pub text: &'a str,
}

impl fmt::Display for Styled<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `anstyle::Style` implements Display for the opening escape,
        // and `Style::render_reset()` for the closing escape.
        if self.style == Style::new() {
            // Pad the text to the requested field width without ANSI
            return f.pad(self.text);
        }
        // When padding is requested we need to pad the *visible* text only,
        // then wrap the whole thing in ANSI codes.
        let width = f.width().unwrap_or(0);
        let padded = format!("{:<width$}", self.text, width = width);
        write!(
            f,
            "{}{}{}",
            self.style.render(),
            padded,
            self.style.render_reset()
        )
    }
}

/// Convenience: wrap text in a style.
pub fn styled(text: &str, style: Style) -> Styled<'_> {
    Styled { style, text }
}

/// Outcome of an `up` invocation, used to render the headline and hint line.
pub enum UpResult {
    /// Fresh daemon was just spawned.
    Fresh,
    /// Daemon was already running and reload found no changes.
    NoChange,
    /// Daemon was already running and reload applied changes. The string is a
    /// pre-rendered, human-readable summary like `"+2 added, 1 changed"`.
    Reloaded(String),
}

/// Info about the post-`up` status block.
pub struct UpStatusInfo<'a> {
    pub service_count: usize,
    pub session_name: Option<&'a str>,
    pub attached: bool,
    pub result: UpResult,
}

/// Print the two-line status block after `up`. Renders a colored glyph + count
/// headline and a dim hint line pointing at the next likely command.
pub fn print_up_status(info: &UpStatusInfo<'_>) -> anyhow::Result<()> {
    let color = use_color();
    let dim = maybe(DIM, color);

    let (glyph, glyph_style) = match info.result {
        UpResult::Fresh | UpResult::NoChange => ("\u{2713}", maybe(GREEN, color)), // ✓
        UpResult::Reloaded(_) => ("\u{21bb}", maybe(YELLOW, color)),               // ↻
    };

    let count_label = if info.service_count == 1 {
        "service"
    } else {
        "services"
    };

    // Build the dim suffix: " · already running", " · +2 added, 1 changed",
    // optionally followed by " · session NAME".
    let mut suffix = String::new();
    match &info.result {
        UpResult::NoChange => suffix.push_str(" · already running"),
        UpResult::Reloaded(summary) => {
            suffix.push_str(" · ");
            suffix.push_str(summary);
        }
        UpResult::Fresh => {}
    }
    if let Some(name) = info.session_name {
        suffix.push_str(&format!(" · session {name}"));
    }

    crate::output::write_line(format_args!(
        "{} {} {}{}",
        styled(glyph, glyph_style),
        info.service_count,
        count_label,
        styled(&suffix, dim),
    ))?;

    // Hint line: dim, two-space indent. Attached `up` will start streaming
    // logs immediately; distinguish ownership from viewing. Detached
    // `up` points at `ps`, plus `logs -f` when output is likely interesting.
    let hint = if info.attached {
        if matches!(info.result, UpResult::Fresh) {
            "  ctrl-c stops the environment".to_string()
        } else {
            "  ctrl-c detaches".to_string()
        }
    } else {
        let mut parts: Vec<&str> = vec!["decompose ps"];
        if !matches!(info.result, UpResult::NoChange) {
            parts.push("decompose logs -f");
        }
        format!("  {}", parts.join(" · "))
    };
    crate::output::write_line(format_args!("{}", styled(&hint, dim)))?;
    Ok(())
}

/// Display initialization separately from child state and readiness.
pub(crate) fn initialization_detail(p: &crate::model::ProcessSnapshot) -> String {
    let detail = p.initialization.detail();
    let detail = if let Some(code) = p.exit_code {
        format!(
            "exit code {code}{}",
            if detail.is_empty() {
                String::new()
            } else {
                format!("; {detail}")
            }
        )
    } else {
        detail
    };
    let detail = if detail.is_empty() {
        p.failure
            .as_ref()
            .map(|d| d.summary.clone())
            .unwrap_or_default()
    } else {
        detail
    };
    if !detail.is_empty() {
        return detail;
    }
    p.initialization_blockers
        .iter()
        .map(|b| {
            let detail = b.initialization.detail();
            format!(
                "waiting for {} initialization{}",
                b.service,
                if detail.is_empty() {
                    String::new()
                } else {
                    format!(": {detail}")
                }
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;

    // These tests mutate process-global env vars so they must run serially.
    // Each test uses a unique var name to avoid cross-contamination.

    #[test]
    fn env_truthy_recognizes_truthy_values() {
        for (i, value) in ["1", "true", "TRUE", "Yes", "on"].iter().enumerate() {
            let key = format!("_DECOMPOSE_ENV_TRUTHY_TEST_POS_{i}");
            // SAFETY: single-threaded test, unique key per iteration.
            unsafe {
                std::env::set_var(&key, value);
            }
            assert!(env_truthy(&key), "expected {value:?} to be truthy");
            unsafe {
                std::env::remove_var(&key);
            }
        }
    }

    #[test]
    fn env_truthy_rejects_falsy_values() {
        for (i, value) in ["0", "false", "no", "", "random"].iter().enumerate() {
            let key = format!("_DECOMPOSE_ENV_TRUTHY_TEST_NEG_{i}");
            unsafe {
                std::env::set_var(&key, value);
            }
            assert!(!env_truthy(&key), "expected {value:?} to be falsy");
            unsafe {
                std::env::remove_var(&key);
            }
        }
    }

    #[test]
    fn env_truthy_returns_false_when_unset() {
        let key = "_DECOMPOSE_ENV_TRUTHY_TEST_UNSET";
        unsafe {
            std::env::remove_var(key);
        }
        assert!(!env_truthy(key));
    }

    #[test]
    fn style_for_status_maps_correctly_without_color() {
        // With color=false all styles should be the identity style.
        for status in &[
            "running",
            "exited",
            "pending",
            "restarting",
            "failed",
            "disabled",
            "not_started",
        ] {
            assert_eq!(
                style_for_status(status, false),
                Style::new(),
                "color=false should always return plain style for {status}"
            );
        }
    }

    #[test]
    fn style_for_status_maps_correctly_with_color() {
        assert_eq!(style_for_status("running", true), GREEN);
        assert_eq!(style_for_status("exited", true), GREEN);
        assert_eq!(style_for_status("pending", true), YELLOW);
        assert_eq!(style_for_status("restarting", true), YELLOW);
        assert_eq!(style_for_status("failed", true), RED);
        assert_eq!(style_for_status("disabled", true), DIM);
        assert_eq!(style_for_status("not_started", true), DIM);
        assert_eq!(style_for_status("stopped", true), DIM);
    }

    #[test]
    fn unified_state_maps_correctly() {
        // running + no probe = healthy (green)
        assert_eq!(
            unified_state("running", false, false, true),
            ("\u{25cf}", "healthy", GREEN)
        );
        // running + probe + healthy = healthy (green)
        assert_eq!(
            unified_state("running", true, true, true),
            ("\u{25cf}", "healthy", GREEN)
        );
        // running + probe + not healthy = running (yellow)
        assert_eq!(
            unified_state("running", true, false, true),
            ("\u{25cb}", "running", YELLOW)
        );
        // pending = yellow
        assert_eq!(
            unified_state("pending", false, false, true),
            ("\u{25cc}", "pending", YELLOW)
        );
        // restarting = yellow
        assert_eq!(
            unified_state("restarting", false, false, true),
            ("\u{25cc}", "restarting", YELLOW)
        );
        // failed = red
        assert_eq!(
            unified_state("failed", false, false, true),
            ("\u{2715}", "failed", RED)
        );
        assert_eq!(
            unified_state("failed_to_start", false, false, true),
            ("\u{2715}", "failed_to_start", RED)
        );
        // stopped / disabled / not_started = dim
        assert_eq!(
            unified_state("stopped", false, false, true),
            ("-", "stopped", DIM)
        );
        assert_eq!(
            unified_state("disabled", false, false, true),
            ("-", "disabled", DIM)
        );
        assert_eq!(
            unified_state("not_started", false, false, true),
            ("-", "not_started", DIM)
        );
        // exited = dim
        assert_eq!(
            unified_state("exited", false, false, true),
            ("-", "exited", DIM)
        );
        // color=false strips ansi style
        assert_eq!(
            unified_state("running", false, false, false),
            ("\u{25cf}", "healthy", Style::new())
        );
    }

    #[test]
    fn styled_display_plain_no_ansi() {
        let s = styled("hello", Style::new());
        assert_eq!(format!("{s}"), "hello");
    }

    #[test]
    fn styled_display_with_width_pads() {
        let s = styled("hi", Style::new());
        assert_eq!(format!("{s:<10}"), "hi        ");
    }
}

pub fn operation_summary(result: &crate::output_model::Acknowledgment) -> String {
    use crate::output_model::ServiceOutcome;
    let mut groups: Vec<(&str, Vec<&str>)> = Vec::new();
    for service in &result.services {
        let verb = match service.outcome {
            ServiceOutcome::StartRequested => "starting",
            ServiceOutcome::RestartRequested => "restarting",
            ServiceOutcome::Registered => "registered",
            ServiceOutcome::AlreadyRunning => "already running:",
            ServiceOutcome::Stopped => "stopped",
            ServiceOutcome::AlreadyStopped => "already stopped:",
            ServiceOutcome::Signalled => "sent signal to",
            ServiceOutcome::Ready => "ready:",
        };
        if let Some((_, names)) = groups.iter_mut().find(|(v, _)| *v == verb) {
            names.push(&service.name);
        } else {
            groups.push((verb, vec![&service.name]));
        }
    }
    if groups.is_empty() {
        return "no eligible services".into();
    }
    groups
        .into_iter()
        .map(|(verb, names)| format!("{verb} {}", names.join(", ")))
        .collect::<Vec<_>>()
        .join("; ")
}

tokio::task_local! {
    pub(crate) static STDOUT: tokio::sync::mpsc::UnboundedSender<Vec<u8>>;
}

/// One command boundary owns the descriptor. Producers enqueue complete records;
/// library users without a boundary write synchronously to process stdout.
pub(crate) fn write_bytes(bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    match STDOUT.try_with(|sender| sender.send(bytes.to_vec())) {
        Ok(result) => result.map_err(|_| std::io::ErrorKind::BrokenPipe.into()),
        Err(_) => std::io::stdout().lock().write_all(bytes),
    }
}

pub(crate) fn write_line(args: std::fmt::Arguments<'_>) -> std::io::Result<()> {
    let mut text = args.to_string();
    text.push('\n');
    write_bytes(text.as_bytes())
}

pub(crate) fn spawn<F>(future: F) -> tokio::task::JoinHandle<F::Output>
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    let sender = STDOUT.try_with(Clone::clone).ok();
    tokio::spawn(async move {
        if let Some(sender) = sender {
            STDOUT.scope(sender, future).await
        } else {
            future.await
        }
    })
}

/// Compact notices share one catalog across terminal UI actions.
pub(crate) enum Notice<'a> {
    Following(bool),
    NothingToYank,
    Yanked { lines: usize, full: bool },
    Match { line: usize, total: usize },
    NoMatch(&'a str),
    ForcingShutdown,
    Stopping,
}
impl std::fmt::Display for Notice<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Following(true) => f.write_str("following"),
            Self::Following(false) => f.write_str("paused"),
            Self::NothingToYank => f.write_str("nothing to yank"),
            Self::Yanked { lines, full } => write!(
                f,
                "yanked {lines} {}line{}{}",
                if *full { "" } else { "visible " },
                if *lines == 1 { "" } else { "s" },
                if *full { " (full buffer)" } else { "" }
            ),
            Self::Match { line, total } => write!(f, "match on line {line} of {total}"),
            Self::NoMatch(query) => write!(f, "no match for /{query}/"),
            Self::ForcingShutdown => f.write_str("forcing shutdown…"),
            Self::Stopping => f.write_str("stopping services… (ctrl-c or Q to force)"),
        }
    }
}
