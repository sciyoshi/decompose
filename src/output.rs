use std::env;
use std::io::IsTerminal;

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

#[cfg(test)]
mod tests {
    use super::env_truthy;

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
            assert!(
                env_truthy(&key),
                "expected {value:?} to be truthy"
            );
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
            assert!(
                !env_truthy(&key),
                "expected {value:?} to be falsy"
            );
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
}
