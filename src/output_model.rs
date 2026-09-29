//! Public, versioned CLI result documents. IPC responses are separate types.
use crate::model::ProcessSnapshot;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DaemonState {
    Running,
    Stopping,
    NotRunning,
    Unreachable,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Daemon {
    pub state: DaemonState,
    pub pid: Option<u32>,
    pub instance: String,
}

#[derive(Debug, Serialize)]
pub struct StatusResult<'a> {
    pub schema_version: &'static str,
    pub daemon: Daemon,
    pub processes: &'a [ProcessSnapshot],
}

impl StatusResult<'_> {
    /// Ordinary running environments need only the process table.
    pub fn summary(&self) -> Option<&'static str> {
        use DaemonState::*;
        let summary = match self.daemon.state {
            NotRunning => "daemon not running",
            Stopping => "daemon stopping",
            Unreachable => "daemon not responding",
            Running if self.processes.is_empty() => "daemon running; no processes configured",
            Running if self.processes.iter().all(|p| p.state == "disabled") => {
                "daemon running; all processes disabled"
            }
            Running
                if self
                    .processes
                    .iter()
                    .all(|p| matches!(p.state.as_str(), "disabled" | "not_started")) =>
            {
                "daemon running; no processes started"
            }
            Running if self.processes.iter().any(|p| p.state == "running") => return None,
            Running
                if self.processes.iter().any(|p| {
                    matches!(p.state.as_str(), "pending" | "initializing" | "restarting")
                }) =>
            {
                "daemon running; processes starting"
            }
            Running => "daemon running; no processes running",
        };
        Some(summary)
    }
}

#[derive(Debug, Serialize)]
pub struct Environment {
    pub instance: String,
    pub daemon: Daemon,
    pub project_dir: Option<std::path::PathBuf>,
    pub config_files: Option<Vec<std::path::PathBuf>>,
    pub process_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<crate::diagnostic::Diagnostic>,
}

#[derive(Debug, Serialize)]
pub struct DiscoveryResult {
    pub schema_version: &'static str,
    pub environments: Vec<Environment>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Accepted,
    Completed,
    Unchanged,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ServiceOutcome {
    StartRequested,
    RestartRequested,
    Registered,
    AlreadyRunning,
    Stopped,
    AlreadyStopped,
    Signalled,
    Ready,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceResult {
    pub name: String,
    pub outcome: ServiceOutcome,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Changes {
    pub added: Vec<String>,
    pub changed: Vec<String>,
    pub removed: Vec<String>,
    pub orphans: Vec<String>,
    pub renamed: Vec<Rename>,
    pub scaled: Vec<Scale>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rename {
    pub from: String,
    pub to: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scale {
    pub service: String,
    pub from_replicas: u16,
    pub to_replicas: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Acknowledgment {
    pub outcome: Outcome,
    pub services: Vec<ServiceResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changes: Option<Changes>,
}

#[derive(Debug, Serialize)]
pub struct OperationResult {
    pub schema_version: &'static str,
    pub operation: String,
    pub daemon: Daemon,
    #[serde(flatten)]
    pub acknowledgment: Acknowledgment,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub daemon_action: Option<DaemonAction>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub readiness: Option<Readiness>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signal: Option<Signal>,
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DaemonAction {
    Started,
    Reused,
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Readiness {
    NotRequested,
    Satisfied,
}
#[derive(Debug, Serialize)]
pub struct Signal {
    pub number: i32,
    pub name: Option<String>,
}

/// Lifecycle payload stored with the same attribution and clock as service logs.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Lifecycle {
    ProcessStarted {
        pid: u32,
    },
    ProcessExited {
        exit_code: Option<i32>,
    },
    ProcessRestartRequested {
        attempt: u32,
        limit: Option<u32>,
    },
    HookStarted,
    HookSkipped,
    HookSucceeded,
    HookFailed,
    HookCancelled,
    Diagnostic {
        diagnostic: Box<crate::diagnostic::Diagnostic>,
    },
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Ownership {
    Owner,
    Viewer,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEvent {
    DaemonStarted { daemon: Daemon },
    ConfigurationReloaded { changes: Changes },
    Attached { ownership: Ownership },
    Detached { ownership: Ownership },
    EnvironmentStopped { ownership: Ownership },
}

impl SessionEvent {
    pub fn text(&self) -> &'static str {
        match self {
            Self::DaemonStarted { .. } => "daemon started",
            Self::ConfigurationReloaded { .. } => "configuration reloaded",
            Self::Attached {
                ownership: Ownership::Owner,
            } => "attached; ctrl-c stops the environment",
            Self::Attached {
                ownership: Ownership::Viewer,
            } => "attached; ctrl-c detaches",
            Self::Detached { .. } => "detached",
            Self::EnvironmentStopped { .. } => "environment stopped",
        }
    }
}
