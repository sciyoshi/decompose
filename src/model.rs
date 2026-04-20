use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};

/// Shared cell holding the current instance name for a running process.
///
/// Daemon tasks (lifecycle, output readers, probes) capture a clone of this
/// handle rather than a plain `String` so that the daemon can rename an
/// instance in place (e.g. scaling from `replicas == 1` to `replicas >= 2`
/// promotes `foo` to `foo[1]`) without invalidating the task's view of which
/// entry in `DaemonState.processes` belongs to it.
///
/// Writes happen under the daemon state lock as part of a reload. Reads are
/// cheap and lock-free on the hot path (they use a standard `RwLock`).
pub type NameHandle = Arc<RwLock<String>>;

/// Construct a new [`NameHandle`] from a starting name.
pub fn make_name_handle(name: String) -> NameHandle {
    Arc::new(RwLock::new(name))
}

/// Read the current name out of a handle. Panics only if the `RwLock` is
/// poisoned, which would indicate a panic in a daemon task under the lock
/// and is not expected in normal operation.
pub fn read_name(handle: &NameHandle) -> String {
    handle.read().expect("NameHandle RwLock poisoned").clone()
}

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
    /// Stable SHA-256 hash of the service's `ProcessConfig`, excluding the
    /// fields that Docker Compose considers changeable-without-recreate
    /// (`depends_on`, `replicas`, `disabled`). Shared across replicas of the
    /// same service. Used by reload to diff services and decide which need
    /// to be restarted. Computed once in `build_process_instances`.
    pub config_hash: String,
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
    /// Readiness flag: set by the readiness probe's success threshold,
    /// cleared by the failure threshold or on process (re)start. This is
    /// what `depends_on: process_healthy` gates on and what `ps` reports
    /// as the HEALTH/STATE indicator.
    ///
    /// Starts `false` on a fresh process; without a configured readiness
    /// probe it remains `false` (see `dependencies_met` for how the
    /// dependency condition handles that case).
    pub ready: bool,
    /// Liveness flag: set by the liveness probe's success threshold, and
    /// cleared when the failure threshold trips (at which point the daemon
    /// SIGKILLs the process so the restart policy re-launches it).
    ///
    /// Defaults to `true` — a process with no liveness probe is assumed
    /// alive. Reset to `true` on (re)start so a new instance starts its
    /// probe cycle with a clean slate.
    pub alive: bool,
    /// Shared-by-reference current instance name. Daemon tasks spawned for
    /// this runtime hold `Arc` clones of this handle. When the daemon
    /// renames an instance in place during a scale 1↔N transition, it
    /// updates this cell (and re-keys the `processes` / `controllers`
    /// maps); tasks then continue looking up their own entry under the
    /// new name without restart.
    pub name_handle: NameHandle,
}

#[derive(Debug, Clone)]
pub struct RuntimePaths {
    pub socket: PathBuf,
    pub pid: PathBuf,
    pub daemon_log: PathBuf,
    pub lock: PathBuf,
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
    /// Readiness-probe pass/fail flag. Drives `depends_on: process_healthy`
    /// and the `ps` HEALTH/STATE column. Without a readiness probe the
    /// daemon leaves this `false`; see `has_readiness_probe` for whether
    /// the service actually has one configured.
    pub ready: bool,
    /// Liveness-probe pass/fail flag. A failing liveness probe triggers a
    /// process restart via SIGKILL. Services without a liveness probe
    /// default to `true` (assumed alive).
    pub alive: bool,
    pub has_readiness_probe: bool,
    /// Whether a liveness probe is configured on the service. Exposed so
    /// downstream consumers can distinguish "no probe, assumed alive" from
    /// "probe configured and passing".
    pub has_liveness_probe: bool,
    pub pid: Option<u32>,
    pub exit_code: Option<i32>,
}
