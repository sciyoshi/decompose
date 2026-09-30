//! Startup progress for redirected output and inline terminals.
use crate::output::{OutputMode, style_for_status, styled, unified_state};
use anyhow::Result;
use std::collections::BTreeMap;
use std::io::IsTerminal;

/// Text progress combines persisted lifecycle events with changing snapshots.
#[derive(Default)]
pub(crate) struct WaitProgress {
    reader: crate::logs::Reader,
    states: std::collections::HashMap<String, String>,
    inline: Option<InlineProgress>,
}

impl WaitProgress {
    pub async fn new(paths: &crate::model::RuntimePaths) -> Result<Self> {
        let mut progress = Self::default();
        if std::io::stdout().is_terminal()
            && std::env::var("TERM").as_deref() != Ok("dumb")
            && crossterm::terminal::size().is_ok_and(|(width, height)| width >= 20 && height >= 4)
        {
            progress.inline = Some(InlineProgress {
                color: crate::output::use_color(),
                ..Default::default()
            });
        } else {
            progress
                .reader
                .poll_records(&paths.daemon_log, &[], Some(0))
                .await?;
        }
        Ok(progress)
    }

    pub fn is_inline(&self) -> bool {
        self.inline.is_some()
    }

    pub fn begin(&mut self) {
        if let Some(inline) = &mut self.inline {
            inline.rows.clear();
        }
    }

    pub fn render(&mut self) -> Result<()> {
        if let Some(inline) = &mut self.inline {
            inline.write(None)?;
        }
        Ok(())
    }

    pub fn finish(&mut self, success: bool) -> Result<()> {
        if let Some(inline) = &mut self.inline {
            inline.write(Some(success))?;
        }
        Ok(())
    }

    pub async fn events(
        &mut self,
        paths: &crate::model::RuntimePaths,
        selected: &std::collections::HashSet<String>,
    ) -> Result<()> {
        // An empty filter means all logs to Reader, but an empty wait set means
        // no services here.
        if selected.is_empty() || self.is_inline() {
            return Ok(());
        }
        let filters = selected.iter().cloned().collect::<Vec<_>>();
        for entry in self
            .reader
            .poll_records(&paths.daemon_log, &filters, None)
            .await?
        {
            if matches!(&entry, crate::logs::LogEntry::Log(record) if record.event.is_some()) {
                entry.write(OutputMode::Table, false)?;
            }
        }
        Ok(())
    }

    pub fn state(&mut self, process: &crate::model::ProcessSnapshot, ready: bool) -> Result<()> {
        let detail = crate::output::initialization_detail(process);
        let stage = process
            .initialization
            .hooks
            .iter()
            .find(|hook| hook.status == "running")
            .and_then(|hook| hook.stage.as_deref());
        if let Some(inline) = &mut self.inline {
            let failed = process.initialization.failure().is_some()
                || matches!(process.state.as_str(), "failed" | "failed_to_start")
                || (process.state == "exited" && !ready);
            let (_, label, mut style) = unified_state(
                &process.state,
                process.has_readiness_probe,
                process.ready,
                inline.color,
            );
            let mut state = if ready {
                style = style_for_status("healthy", inline.color);
                "ready".into()
            } else if failed {
                style = style_for_status("failed", inline.color);
                if detail.is_empty() {
                    process.state.clone()
                } else {
                    detail.clone()
                }
            } else if !detail.is_empty() {
                style = style_for_status("initializing", inline.color);
                detail.clone()
            } else if process.state == "running" && !process.ready && process.has_readiness_probe {
                "waiting for readiness".into()
            } else {
                label.into()
            };
            if !ready && let Some(stage) = stage {
                state.push_str(&format!("; {stage}"));
            }
            let hooks = &process.initialization.hooks;
            if !ready && !hooks.is_empty() {
                let done = hooks
                    .iter()
                    .filter(|hook| {
                        hook.status == "succeeded"
                            || (hook.status == "skipped"
                                && hook.reason.as_deref() == Some("already_satisfied"))
                    })
                    .count();
                if done < hooks.len() {
                    state.push_str(&format!("; hooks {done}/{}", hooks.len()));
                }
            }
            inline.rows.insert(
                process.name.clone(),
                Row {
                    state,
                    ready,
                    failed,
                    style,
                },
            );
            return Ok(());
        }
        let mut state = process.state.clone();
        if ready {
            state.push_str("; ready");
        } else if process.state == "running" && process.has_readiness_probe && !process.ready {
            state.push_str("; waiting for readiness");
        }
        if !detail.is_empty() {
            state.push_str("; ");
            state.push_str(&detail);
        }
        if let Some(stage) = stage {
            state.push_str("; ");
            state.push_str(stage);
        }
        if self.states.get(&process.name) != Some(&state) {
            crate::output::write_line(format_args!("[{}] {state}", process.name))?;
            self.states.insert(process.name.clone(), state);
        }
        Ok(())
    }
}

struct Row {
    state: String,
    ready: bool,
    failed: bool,
    style: anstyle::Style,
}

/// Owns only the lines it printed. No alternate screen, raw mode, cursor hiding,
/// or screen clearing: the final frame remains above the shell prompt.
#[derive(Default)]
struct InlineProgress {
    rows: BTreeMap<String, Row>,
    previous_lines: usize,
    previous_size: Option<(u16, u16)>,
    frame: usize,
    color: bool,
}

impl InlineProgress {
    fn write(&mut self, finished: Option<bool>) -> Result<()> {
        let size = crossterm::terminal::size().unwrap_or((80, 24));
        let frame = self.frame(size, finished);
        crate::output::write_bytes(frame.as_bytes())?;
        Ok(())
    }

    fn frame(&mut self, size: (u16, u16), finished: Option<bool>) -> String {
        // Leave a column for terminals' automatic wrapping, and a row for the
        // cursor below the display. Never move up into pre-existing output.
        let width = usize::from(size.0.saturating_sub(1));
        let height = usize::from(size.1.saturating_sub(1));
        if width == 0 || height == 0 {
            self.previous_lines = 0;
            self.previous_size = Some(size);
            return String::new();
        }
        let ready = self.rows.values().filter(|row| row.ready).count();
        let failed = self.rows.values().filter(|row| row.failed).count();
        let total = self.rows.len();
        let filled = (ready * 12).checked_div(total).unwrap_or(0);
        let title = match finished {
            Some(true) => "Ready",
            Some(false) if failed > 0 => "Startup failed",
            Some(false) => "Wait ended",
            None => "Starting",
        };
        let summary = format!(
            "{title} [{}{}] {ready}/{total} ready{}",
            "=".repeat(filled),
            "-".repeat(12 - filled),
            if failed == 0 {
                String::new()
            } else {
                format!("; {failed} failed")
            }
        );
        let summary_style = style_for_status(
            if failed > 0 {
                "failed"
            } else if finished == Some(true) {
                "healthy"
            } else {
                "pending"
            },
            self.color,
        );
        let mut lines = vec![styled(&fit_line(&summary, width), summary_style).to_string()];
        let mut rows = self.rows.iter().collect::<Vec<_>>();
        let capacity = height.saturating_sub(1);
        let visible = if rows.len() > capacity {
            capacity.saturating_sub(1)
        } else {
            rows.len()
        };
        if visible < rows.len() {
            // Keep failures and unfinished services visible in short terminals.
            rows.sort_by_key(|(_, row)| (row.ready, !row.failed));
        }
        let spinner = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"][self.frame % 10];
        let name_width = rows
            .iter()
            .map(|(name, _)| ratatui::text::Span::raw(name.as_str()).width())
            .max()
            .unwrap_or(0)
            .min(24)
            .min(width / 3);
        for (name, row) in rows.iter().take(visible) {
            let marker = if row.failed {
                "✗"
            } else if row.ready {
                "✓"
            } else if finished.is_some() {
                "-"
            } else {
                spinner
            };
            let name = fit_line(name, name_width);
            let padding =
                " ".repeat(name_width.saturating_sub(ratatui::text::Span::raw(&name).width()));
            let prefix = format!("  {marker} {name}{padding}  ");
            let prefix_width = ratatui::text::Span::raw(&prefix).width();
            if prefix_width >= width {
                lines.push(fit_line(&prefix, width));
            } else {
                let state = fit_line(&row.state, width - prefix_width);
                lines.push(format!(
                    "  {} {name}{padding}  {}",
                    styled(marker, row.style),
                    styled(&state, row.style)
                ));
            }
        }
        if visible < rows.len() && lines.len() < height {
            lines.push(fit_line(
                &format!("  … {} more services", rows.len() - visible),
                width,
            ));
        }
        let mut output = String::new();
        // Resizing may reflow earlier rows. Start a new block rather than risk
        // overwriting shell history using stale cursor coordinates.
        let previous = if self.previous_size == Some(size) {
            self.previous_lines
        } else {
            0
        };
        if previous > 0 {
            output.push_str(&format!("\x1b[{previous}A"));
        }
        let count = previous.max(lines.len());
        for index in 0..count {
            output.push_str("\r\x1b[2K");
            if let Some(line) = lines.get(index) {
                output.push_str(line);
            }
            output.push('\n');
        }
        self.previous_lines = count;
        self.previous_size = Some(size);
        self.frame = self.frame.wrapping_add(1);
        output
    }
}

fn fit_line(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    // Service names and hook errors are configuration/application data. Remove
    // controls so they cannot move the cursor or create extra display rows.
    let clean = text.chars().filter(|c| !c.is_control()).collect::<String>();
    if ratatui::text::Span::raw(&clean).width() <= width {
        return clean;
    }
    let mut result = String::new();
    let mut used = 0;
    for ch in clean.chars() {
        let columns = ratatui::text::Span::raw(ch.to_string()).width();
        if used + columns >= width {
            break;
        }
        result.push(ch);
        used += columns;
    }
    result.push('…');
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn display() -> InlineProgress {
        let mut display = InlineProgress::default();
        for (name, ready, failed) in [
            ("api", false, false),
            ("db", true, false),
            ("worker", false, true),
        ] {
            display.rows.insert(
                name.into(),
                Row {
                    state: if ready {
                        "ready"
                    } else if failed {
                        "failed"
                    } else {
                        "initializing post_start:migrate; executing"
                    }
                    .into(),
                    ready,
                    failed,
                    style: anstyle::Style::new(),
                },
            );
        }
        display
    }

    #[test]
    fn inline_redraw_stays_within_its_own_lines_and_leaves_final_state() {
        let mut display = display();
        let first = display.frame((100, 24), None);
        assert!(!first.contains("A"));
        assert!(first.contains("1/3 ready; 1 failed"));
        assert!(first.contains("⠋ api"));
        assert!(first.contains("✓ db"));
        assert!(first.contains("✗ worker"));
        let second = display.frame((100, 24), None);
        assert!(second.starts_with("\x1b[4A"));
        assert!(second.contains("⠙ api"));
        let final_frame = display.frame((100, 24), Some(false));
        assert!(final_frame.contains("Startup failed"));
        assert!(final_frame.contains("- api"));
        assert!(final_frame.ends_with('\n'));
        for frame in [first, second, final_frame] {
            assert!(!frame.contains("\x1b[2J"));
            assert!(!frame.contains("\x1b[?"));
            assert_eq!(frame.lines().count(), 4);
        }
    }

    #[test]
    fn small_terminal_prioritizes_failures_and_resize_does_not_move_into_history() {
        let mut display = display();
        display.frame((100, 24), None);
        let small = display.frame((24, 4), None);
        assert!(!small.contains("A"));
        assert!(small.contains("✗ worker"));
        assert!(small.contains("2 more services"));
        assert_eq!(small.lines().count(), 3);
        let next = display.frame((24, 4), None);
        assert!(next.starts_with("\x1b[3A"));
    }

    #[test]
    fn long_unicode_and_control_characters_cannot_wrap_or_move_cursor() {
        for width in 0..20 {
            let text = fit_line("界界界 long\nname\r\t\x1b[2J", width);
            assert!(ratatui::text::Span::raw(&text).width() <= width);
            assert!(!text.chars().any(char::is_control));
        }
        assert_eq!(fit_line("api", 3), "api");
    }
}
