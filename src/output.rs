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
    #[test]
    fn env_truthy_parses_common_values() {
        assert!(env_truthy_from("TRUE"));
        assert!(env_truthy_from("1"));
        assert!(env_truthy_from("yes"));
        assert!(!env_truthy_from("false"));
        assert!(!env_truthy_from("0"));
    }

    fn env_truthy_from(input: &str) -> bool {
        matches!(
            input.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    }
}
