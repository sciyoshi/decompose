//! Startup reports for the OSC 7501 Program Status Protocol.
//! Reports describe the client's operation, never the daemon's lifetime.
use std::io::IsTerminal;

use base64::{Engine as _, engine::general_purpose::STANDARD};

use crate::output::{self, OutputMode};

#[derive(Clone, Copy)]
enum State {
    Working,
    Done,
    Error,
    Idle,
}

struct Report<'a> {
    state: State,
    message: &'a str,
    progress: Option<u8>,
}

impl Report<'_> {
    fn encode(&self) -> Vec<u8> {
        let state = match self.state {
            State::Working => "working",
            State::Done => "done",
            State::Error => "error",
            State::Idle => "idle",
        };
        // Messages must be single-line UTF-8 without C0/C1 controls, at most
        // 2048 decoded bytes. Truncate on character boundaries before base64.
        let mut message = String::new();
        for ch in self.message.chars() {
            let ch = if ch.is_control() { ' ' } else { ch };
            if message.len() + ch.len_utf8() > 2048 {
                break;
            }
            message.push(ch);
        }
        let mut sequence = format!(
            "\x1b]7501;state={state}:app=decompose:msg={}",
            STANDARD.encode(message)
        );
        if let State::Working = self.state
            && let Some(progress) = self.progress
        {
            sequence.push_str(&format!(":progress={}", progress.min(100)));
        }
        sequence.push_str("\x1b\\");
        sequence.into_bytes()
    }
}

fn enabled(mode: OutputMode, terminal: bool, dumb: bool, disabled: bool) -> bool {
    mode == OutputMode::Table && terminal && !dumb && !disabled
}

pub(crate) struct Reporter {
    enabled: bool,
    finished: bool,
    last: Vec<u8>,
}

impl Reporter {
    pub fn new(mode: OutputMode) -> Self {
        Self {
            enabled: enabled(
                mode,
                std::io::stdout().is_terminal(),
                std::env::var("TERM").as_deref() == Ok("dumb"),
                output::env_truthy("DECOMPOSE_NO_PROGRAM_STATUS"),
            ),
            finished: false,
            last: Vec::new(),
        }
    }

    fn report(&mut self, state: State, message: &str, progress: Option<u8>) {
        if !self.enabled || self.finished {
            return;
        }
        let bytes = Report {
            state,
            message,
            progress,
        }
        .encode();
        if bytes != self.last {
            // Status is supplementary: a terminal write must not prevent
            // startup or replace the original operation's diagnostic.
            let _ = output::write_bytes(&bytes);
            self.last = bytes;
        }
    }

    pub fn working(&mut self, message: &str, progress: Option<u8>) {
        self.report(State::Working, message, progress);
    }

    pub fn readiness(&mut self, ready: usize, total: usize, detail: Option<&str>) {
        let mut message = format!("Starting environment: {ready}/{total} services ready");
        if let Some(detail) = detail {
            message.push_str("; ");
            message.push_str(detail);
        }
        let progress = if total == 0 {
            100
        } else {
            ((ready as u128 * 100) / total as u128).min(100) as u8
        };
        self.working(&message, Some(progress));
    }

    pub fn done(&mut self, message: &str) {
        self.report(State::Done, message, None);
        self.finished = true;
    }

    pub fn cancel(&mut self, message: &str) {
        self.report(State::Idle, message, None);
        self.finished = true;
    }

    pub fn fail(&mut self, error: &anyhow::Error) {
        let diagnostic = crate::diagnostic::Diagnostic::from_error(error);
        if diagnostic.code == "interrupted" {
            self.cancel(&diagnostic.summary);
        } else {
            self.report(State::Error, &diagnostic.summary, None);
            self.finished = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_are_bounded_valid_utf8_without_controls() {
        let message = format!("hello\n\t\x1b\u{7f}\u{85}{}", "🦀".repeat(1000));
        let bytes = Report {
            state: State::Working,
            message: &message,
            progress: Some(50),
        }
        .encode();
        assert!(bytes.len() <= 4096);
        let sequence = String::from_utf8(bytes).unwrap();
        assert!(sequence.starts_with("\x1b]7501;state=working:app=decompose:msg="));
        let (message, suffix) = sequence
            .split_once("msg=")
            .unwrap()
            .1
            .split_once(':')
            .unwrap();
        assert_eq!(suffix, "progress=50\x1b\\");
        assert!(message.len() <= 2732);
        let decoded = String::from_utf8(STANDARD.decode(message).unwrap()).unwrap();
        assert!(decoded.len() <= 2048);
        assert!(decoded.starts_with("hello     🦀"));
        assert!(!decoded.chars().any(char::is_control));
    }

    #[test]
    fn only_interactive_text_output_enables_reporting() {
        assert!(enabled(OutputMode::Table, true, false, false));
        assert!(!enabled(OutputMode::Json, true, false, false));
        assert!(!enabled(OutputMode::Table, false, false, false));
        assert!(!enabled(OutputMode::Table, true, true, false));
        assert!(!enabled(OutputMode::Table, true, false, true));
    }

    #[tokio::test]
    async fn updates_use_output_sink_deduplicate_and_finish_once() {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        output::STDOUT
            .scope(sender, async {
                let mut reporter = Reporter {
                    enabled: true,
                    finished: false,
                    last: Vec::new(),
                };
                reporter.readiness(1, 2, Some("api: waiting for readiness"));
                reporter.readiness(1, 2, Some("api: waiting for readiness"));
                reporter.done("Environment ready");
                reporter.fail(&anyhow::anyhow!("later TUI error"));
                reporter.working("later update", None);
            })
            .await;
        let working = String::from_utf8(receiver.try_recv().unwrap()).unwrap();
        assert!(working.contains("state=working"));
        assert!(working.contains(":progress=50"));
        let done = String::from_utf8(receiver.try_recv().unwrap()).unwrap();
        assert!(done.contains("state=done:app=decompose"));
        assert!(!done.contains("progress="));
        assert!(receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn cancellation_is_idle_and_failure_is_error() {
        for (error, state) in [
            (
                crate::diagnostic::Diagnostic::error("interrupted", "wait cancelled").into(),
                "idle",
            ),
            (anyhow::anyhow!("invalid config"), "error"),
        ] {
            let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
            output::STDOUT
                .scope(sender, async {
                    let mut reporter = Reporter {
                        enabled: true,
                        finished: false,
                        last: Vec::new(),
                    };
                    reporter.fail(&error);
                })
                .await;
            let sequence = String::from_utf8(receiver.try_recv().unwrap()).unwrap();
            assert!(sequence.contains(&format!("state={state}:app=decompose")));
        }
    }
}
