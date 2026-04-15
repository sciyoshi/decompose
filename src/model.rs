use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DependencyCondition {
    #[default]
    ProcessStarted,
    ProcessCompleted,
    ProcessCompletedSuccessfully,
    ProcessHealthy,
    ProcessLogReady,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RestartPolicy {
    #[default]
    No,
    OnFailure,
    Always,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitMode {
    #[default]
    WaitAll,
    ExitOnFailure,
    ExitOnEnd,
}

#[derive(Debug, Clone)]
pub struct ProcessInstanceSpec {
    pub name: String,
    pub base_name: String,
    pub replica: u16,
    pub command: String,
    pub description: Option<String>,
    pub working_dir: PathBuf,
    pub environment: BTreeMap<String, String>,
    pub depends_on: BTreeMap<String, DependencyCondition>,
    pub ready_log_line: Option<String>,
    pub restart_policy: RestartPolicy,
    pub backoff_seconds: u64,
    pub max_restarts: Option<u32>,
    pub shutdown_signal: Option<i32>,
    pub shutdown_timeout_seconds: u64,
    pub shutdown_command: Option<String>,
    pub readiness_probe: Option<HealthProbe>,
    pub liveness_probe: Option<HealthProbe>,
    pub disabled: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HealthProbe {
    #[serde(default)]
    pub exec: Option<ExecCheck>,
    #[serde(default)]
    pub http_get: Option<HttpCheck>,
    #[serde(default = "default_period")]
    pub period_seconds: u64,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
    #[serde(default = "default_initial_delay")]
    pub initial_delay_seconds: u64,
    #[serde(default = "default_success_threshold")]
    pub success_threshold: u32,
    #[serde(default = "default_failure_threshold")]
    pub failure_threshold: u32,
}

fn default_period() -> u64 {
    10
}

fn default_timeout() -> u64 {
    1
}

fn default_initial_delay() -> u64 {
    0
}

fn default_success_threshold() -> u32 {
    1
}

fn default_failure_threshold() -> u32 {
    3
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ExecCheck {
    pub command: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HttpCheck {
    #[serde(default = "default_host")]
    pub host: String,
    pub port: u16,
    #[serde(default = "default_scheme")]
    pub scheme: String,
    #[serde(default = "default_path")]
    pub path: String,
}

fn default_host() -> String {
    "127.0.0.1".to_string()
}

fn default_scheme() -> String {
    "http".to_string()
}

fn default_path() -> String {
    "/".to_string()
}

#[derive(Debug, Clone)]
pub enum ProcessStatus {
    /// Defined in config but not yet selected for launch (e.g. not part of
    /// the initial `up` subset). Will not be started by the supervisor until
    /// explicitly requested via `start` or `up`.
    NotStarted,
    Pending,
    Running {
        pid: u32,
    },
    Exited {
        code: i32,
    },
    FailedToStart {
        reason: String,
    },
    Stopped,
    Restarting,
    Disabled,
}

impl ProcessStatus {
    pub fn to_human(&self) -> String {
        match self {
            ProcessStatus::NotStarted => "not_started".to_string(),
            ProcessStatus::Pending => "pending".to_string(),
            ProcessStatus::Running { pid } => format!("running(pid={pid})"),
            ProcessStatus::Exited { code } => format!("exited(code={code})"),
            ProcessStatus::FailedToStart { reason } => format!("failed_to_start({reason})"),
            ProcessStatus::Stopped => "stopped".to_string(),
            ProcessStatus::Restarting => "restarting".to_string(),
            ProcessStatus::Disabled => "disabled".to_string(),
        }
    }

    pub fn to_json_status(&self) -> &'static str {
        match self {
            ProcessStatus::NotStarted => "not_started",
            ProcessStatus::Pending => "pending",
            ProcessStatus::Running { .. } => "running",
            ProcessStatus::Exited { code: 0 } => "exited",
            ProcessStatus::Exited { .. } => "failed",
            ProcessStatus::FailedToStart { .. } => "failed",
            ProcessStatus::Stopped => "stopped",
            ProcessStatus::Restarting => "restarting",
            ProcessStatus::Disabled => "disabled",
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            ProcessStatus::NotStarted
                | ProcessStatus::Exited { .. }
                | ProcessStatus::FailedToStart { .. }
                | ProcessStatus::Stopped
                | ProcessStatus::Disabled
        )
    }
}

#[derive(Debug, Clone)]
pub struct ProcessRuntime {
    pub spec: ProcessInstanceSpec,
    pub status: ProcessStatus,
    pub started_once: bool,
    pub log_ready: bool,
    pub restart_count: u32,
    pub healthy: bool,
}

#[derive(Debug, Clone)]
pub struct RuntimePaths {
    pub socket: PathBuf,
    pub pid: PathBuf,
    pub daemon_log: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessSnapshot {
    pub name: String,
    pub base: String,
    pub replica: u16,
    pub status: String,
    /// Unified status field for JSON consumers.
    pub state: String,
    pub description: Option<String>,
    pub restart_count: u32,
    pub log_ready: bool,
    pub healthy: bool,
    pub has_readiness_probe: bool,
    pub exit_code: Option<i32>,
}
