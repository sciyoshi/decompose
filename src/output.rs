use std::env;
use std::fmt;
use std::io::IsTerminal;

use anstyle::{AnsiColor, Style};
use clap::Args;
use serde::Serialize;

#[derive(Args, Debug, Clone, Default)]
pub struct OutputArgs {
    /// Emit JSON output.
    #[arg(long, conflicts_with = "table")]
    pub json: bool,
    /// Emit table/text output.
    #[arg(long, conflicts_with = "json")]
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
        if self.table {
            return OutputMode::Table;
        }
        if std::io::stdout().is_terminal() || env_truthy("LLM") || env_truthy("CI") {
            OutputMode::Table
        } else {
            OutputMode::Json
        }
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

pub fn print_json<T: Serialize>(value: &T) {
    if let Ok(encoded) = serde_json::to_string(value) {
        println!("{encoded}");
    } else {
        println!("{{\"error\":\"failed to serialize json output\"}}");
    }
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
    if let Some(val) = env::var_os("NO_COLOR") {
        if !val.is_empty() {
            return false;
        }
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
        "pending" | "starting" | "restarting" => YELLOW,
        "failed" | "failed_to_start" => RED,
        "disabled" | "not_started" | "stopped" => DIM,
        _ => Style::new(),
    };
    maybe(s, color)
}

/// Single-char state glyph for a row, with the style it should be rendered in.
///
/// Used as a leading column in `ps` so each row has a quick visual indicator.
pub fn glyph_for_state(state: &str, color: bool) -> (&'static str, Style) {
    let (g, s) = match state {
        "running" | "exited" => ("\u{2713}", GREEN), // ✓
        "pending" | "starting" | "restarting" => ("\u{2026}", YELLOW), // …
        "failed" | "failed_to_start" => ("\u{2717}", RED), // ✗
        "stopped" | "disabled" | "not_started" => ("\u{2014}", DIM), // —
        _ => ("\u{00b7}", Style::new()),             // ·
    };
    (g, maybe(s, color))
}

/// Renders the HEALTH column as a glyph plus style, given (has_probe, healthy).
///
/// - has probe + healthy   → ✓ green
/// - has probe + failing   → ✗ red
/// - no probe configured   → — dim
pub fn glyph_for_health(has_probe: bool, healthy: bool, color: bool) -> (&'static str, Style) {
    let (g, s) = match (has_probe, healthy) {
        (true, true) => ("\u{2713}", GREEN), // ✓
        (true, false) => ("\u{2717}", RED),  // ✗
        (false, _) => ("\u{2014}", DIM),     // —
    };
    (g, maybe(s, color))
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

/// Info about the footer to print after `up` completes.
pub struct FooterInfo<'a> {
    pub service_count: usize,
    pub process_count: usize,
    pub session_name: Option<&'a str>,
    pub socket_path: &'a std::path::Path,
    pub attached: bool,
}

/// Print the footer block after `up`.
pub fn print_footer(info: &FooterInfo<'_>) {
    let color = use_color();

    // Line 1: "N services · M processes · session NAME          ctrl-c detaches"
    let mut left = format!(
        "{} {} · {} {}",
        info.service_count,
        if info.service_count == 1 {
            "service"
        } else {
            "services"
        },
        info.process_count,
        if info.process_count == 1 {
            "process"
        } else {
            "processes"
        },
    );
    if let Some(name) = info.session_name {
        left.push_str(&format!(" · session {name}"));
    }

    if info.attached {
        let hint = "ctrl-c detaches";
        let dim_style = maybe(DIM, color);
        println!("{left}    {}", styled(hint, dim_style),);
    } else {
        println!("{left}");
    }

    // Line 2: "daemon supervising · socket PATH"
    let socket_display = shorten_socket_path(info.socket_path);
    println!("daemon supervising · socket {socket_display}");
}

/// Replace the `$XDG_RUNTIME_DIR` prefix in a socket path with the literal
/// env-var reference, keeping output portable.
fn shorten_socket_path(path: &std::path::Path) -> String {
    if let Some(xdg) = env::var_os("XDG_RUNTIME_DIR") {
        let xdg_path = std::path::Path::new(&xdg);
        if let Ok(suffix) = path.strip_prefix(xdg_path) {
            return format!("$XDG_RUNTIME_DIR/{}", suffix.display());
        }
    }
    path.display().to_string()
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
    fn glyph_for_state_maps_correctly() {
        assert_eq!(glyph_for_state("running", true), ("\u{2713}", GREEN));
        assert_eq!(glyph_for_state("exited", true), ("\u{2713}", GREEN));
        assert_eq!(glyph_for_state("pending", true), ("\u{2026}", YELLOW));
        assert_eq!(glyph_for_state("starting", true), ("\u{2026}", YELLOW));
        assert_eq!(glyph_for_state("failed", true), ("\u{2717}", RED));
        assert_eq!(glyph_for_state("failed_to_start", true), ("\u{2717}", RED));
        assert_eq!(glyph_for_state("stopped", true), ("\u{2014}", DIM));
        assert_eq!(glyph_for_state("disabled", true), ("\u{2014}", DIM));
        // color=false strips ansi style
        assert_eq!(
            glyph_for_state("running", false),
            ("\u{2713}", Style::new())
        );
    }

    #[test]
    fn glyph_for_health_covers_all_cases() {
        assert_eq!(glyph_for_health(true, true, true), ("\u{2713}", GREEN));
        assert_eq!(glyph_for_health(true, false, true), ("\u{2717}", RED));
        assert_eq!(glyph_for_health(false, false, true), ("\u{2014}", DIM));
        assert_eq!(glyph_for_health(false, true, true), ("\u{2014}", DIM));
        // color=false
        assert_eq!(
            glyph_for_health(true, true, false),
            ("\u{2713}", Style::new())
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

    #[test]
    fn shorten_socket_path_substitutes_xdg_prefix() {
        unsafe {
            std::env::set_var("XDG_RUNTIME_DIR", "/run/user/1000");
        }
        let path = std::path::Path::new("/run/user/1000/decompose/abc.sock");
        let result = shorten_socket_path(path);
        assert_eq!(result, "$XDG_RUNTIME_DIR/decompose/abc.sock");
        unsafe {
            std::env::remove_var("XDG_RUNTIME_DIR");
        }
    }

    #[test]
    fn shorten_socket_path_keeps_absolute_when_no_xdg() {
        unsafe {
            std::env::remove_var("XDG_RUNTIME_DIR");
        }
        let path = std::path::Path::new("/tmp/decompose/abc.sock");
        let result = shorten_socket_path(path);
        assert_eq!(result, "/tmp/decompose/abc.sock");
    }
}
