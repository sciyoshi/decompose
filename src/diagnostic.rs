//! Serializable command diagnostics shared by the CLI and remote callers.
use std::io::Write;

use serde::{Deserialize, Serialize};

use crate::output::OutputMode;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Diagnostic {
    pub schema_version: String,
    pub severity: String,
    pub code: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub causes: Vec<DiagnosticCause>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<DiagnosticDetails>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<DiagnosticContext>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<Source>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticCause {
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub os_code: Option<i32>,
}

impl Diagnostic {
    pub fn failures(code: &str, summary: &str, failures: Vec<Self>) -> Self {
        let mut diagnostic = Self::error(code, summary);
        diagnostic.details = Some(DiagnosticDetails::Failures { failures });
        diagnostic
    }

    pub fn error(code: &str, summary: impl Into<String>) -> Self {
        Self {
            schema_version: "1.0".into(),
            severity: "error".into(),
            code: code.into(),
            summary: summary.into(),
            causes: Vec::new(),
            usage: None,
            details: None,
            hint: None,
            context: None,
            source: None,
        }
    }

    pub fn from_error(error: &anyhow::Error) -> Self {
        if let Some(diagnostic) = error.downcast_ref::<Self>() {
            let mut result = diagnostic.clone();
            if error.to_string() != result.summary {
                let mut causes = error
                    .chain()
                    .skip(1)
                    .map(|cause| DiagnosticCause {
                        message: cause.to_string(),
                        os_code: cause
                            .downcast_ref::<std::io::Error>()
                            .and_then(|e| e.raw_os_error()),
                    })
                    .collect::<Vec<_>>();
                causes.extend(result.causes);
                result.summary = error.to_string();
                result.causes = causes;
            }
            return result;
        }
        let code = if error.is::<tokio::time::error::Elapsed>() {
            "ipc_timeout"
        } else if error.is::<serde_json::Error>() {
            "serialization_failed"
        } else {
            "operation_failed"
        };
        let mut diagnostic = error
            .downcast_ref::<LocalError>()
            .map(|e| e.diagnostic.clone())
            .unwrap_or_else(|| Self::error(code, error.to_string()));
        diagnostic.summary = error.to_string();
        diagnostic.causes = error
            .chain()
            .skip(1)
            .map(|cause| DiagnosticCause {
                message: cause.to_string(),
                os_code: cause
                    .downcast_ref::<std::io::Error>()
                    .and_then(|e| e.raw_os_error()),
            })
            .collect();
        diagnostic
    }

    /// The caller deliberately ignores a diagnostic-write failure: reporting it
    /// recursively would hide the original error and cannot repair stderr.
    pub fn write(&self, mode: OutputMode, writer: &mut impl Write) -> anyhow::Result<()> {
        if mode == OutputMode::Json {
            let encoded = serde_json::to_vec(self)?;
            writer.write_all(&encoded)?;
            writer.write_all(b"\n")?;
        } else {
            writeln!(writer, "{}: {}", self.severity, self.summary)?;
            if let Some(source) = &self.source {
                writeln!(writer, "  source: {}", source.path.display())?;
            }
            if let Some(context) = &self.context {
                if let Some(command) = &context.command {
                    writeln!(writer, "  command: {command}")?;
                }
                if let Some(path) = &context.working_dir {
                    writeln!(writer, "  working directory: {}", path.display())?;
                }
            }
            for cause in &self.causes {
                writeln!(writer, "  caused by: {}", cause.message)?;
            }
            if let Some(DiagnosticDetails::Failures { failures }) = &self.details {
                for failure in failures {
                    writeln!(writer, "  {}", failure.summary)?;
                    for cause in &failure.causes {
                        writeln!(writer, "    caused by: {}", cause.message)?;
                    }
                }
            }
            if let Some(hint) = &self.hint {
                writeln!(writer, "  hint: {hint}")?;
            }
            if let Some(usage) = &self.usage {
                writeln!(writer, "\n{usage}")?;
            }
        }
        Ok(())
    }
}

impl std::fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.summary)
    }
}
impl std::error::Error for Diagnostic {}

tokio::task_local! {
    pub(crate) static WARNINGS: std::cell::RefCell<Vec<Diagnostic>>;
}

/// Validation is independent of terminal rendering. CLI invocations collect
/// warnings, deduplicate repeated validation, and render at their boundary.
pub fn warning(code: &str, summary: impl Into<String>) {
    let mut diagnostic = Diagnostic::error(code, summary);
    diagnostic.severity = "warning".into();
    if WARNINGS
        .try_with(|warnings| {
            let mut warnings = warnings.borrow_mut();
            if !warnings
                .iter()
                .any(|d| d.code == diagnostic.code && d.summary == diagnostic.summary)
            {
                warnings.push(diagnostic.clone());
            }
        })
        .is_err()
    {
        let _ = diagnostic.write(OutputMode::Table, &mut std::io::stderr());
    }
}

/// Code-specific details remain typed even when their serialized fields differ.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum DiagnosticDetails {
    Readiness {
        daemon: crate::output_model::Daemon,
        pending: Vec<crate::model::ProcessSnapshot>,
        failures: Vec<crate::model::ProcessSnapshot>,
    },
    Startup {
        daemon_log: std::path::PathBuf,
        log_excerpt: Vec<String>,
        excerpt_truncated: bool,
    },
    Targets {
        requested: Vec<String>,
        known: Vec<String>,
    },
    PartialOperation {
        daemon: crate::output_model::Daemon,
        accepted: Vec<crate::output_model::ServiceResult>,
        completed: Vec<crate::output_model::ServiceResult>,
        failures: Vec<Diagnostic>,
    },
    Failures {
        failures: Vec<Diagnostic>,
    },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DiagnosticContext {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub process: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<std::path::PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hook_phase: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hook_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hook_stage: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Source {
    pub path: std::path::PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub range: Option<SourceRange>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceRange {
    pub start: Position,
    pub end: Position,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Position {
    pub line: usize,
    pub column: usize,
}

/// Classification metadata travels with the original local source error.
#[derive(Debug)]
pub struct LocalError {
    pub diagnostic: Diagnostic,
    pub cause: anyhow::Error,
}
impl std::fmt::Display for LocalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.diagnostic.summary)
    }
}
impl std::error::Error for LocalError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.cause.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn partial_results_survive_remote_serialization_without_losing_completed_work() {
        let mut diagnostic =
            Diagnostic::error("process_signal_failed", "failed to signal services");
        diagnostic.details = Some(DiagnosticDetails::PartialOperation {
            daemon: crate::output_model::Daemon {
                state: crate::output_model::DaemonState::Running,
                pid: Some(42),
                instance: "test".into(),
            },
            accepted: Vec::new(),
            completed: vec![crate::output_model::ServiceResult {
                name: "api".into(),
                outcome: crate::output_model::ServiceOutcome::Signalled,
            }],
            failures: vec![Diagnostic::error("process_signal_failed", "worker")],
        });
        let encoded = serde_json::to_vec(&diagnostic).unwrap();
        let decoded: Diagnostic = serde_json::from_slice(&encoded).unwrap();
        let Some(DiagnosticDetails::PartialOperation { completed, .. }) = decoded.details else {
            panic!("partial results were lost");
        };
        assert_eq!(completed[0].name, "api");
    }
}
