use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

use crate::output::OutputArgs;

#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about,
    after_help = "\
Quick start:
  decompose up          Start all services and attach
  decompose up -d       Start detached
  decompose ps          Show running processes
  decompose logs -f     Follow logs
  decompose down        Stop everything

Config files: decompose.yml, decompose.yaml, compose.yml, compose.yaml
Docs: https://github.com/sciyoshi/decompose"
)]
pub struct Cli {
    /// Config file path(s). If omitted, auto-discovery is used. Can be repeated.
    #[arg(long = "file", global = true)]
    pub config_files: Vec<PathBuf>,
    /// Session/project name override for instance identity.
    #[arg(
        long = "session",
        alias = "project-name",
        env = "DECOMPOSE_SESSION",
        global = true
    )]
    pub session: Option<String>,
    /// Additional .env files to load.
    #[arg(short = 'e', long = "env-file", global = true)]
    pub env_files: Vec<PathBuf>,
    /// Disable automatic .env file loading.
    #[arg(long = "disable-dotenv", global = true)]
    pub disable_dotenv: bool,
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Start the environment.
    Up(UpArgs),
    /// Stop the environment daemon and its managed processes.
    Down(DownArgs),
    /// Show process status.
    Ps(OutputOnlyArgs),
    /// Reattach to a running environment's logs.
    Attach(OutputOnlyArgs),
    /// View process logs.
    Logs(LogsArgs),
    /// Start one or more previously-stopped services. With no args, starts all.
    Start(ServiceArgs),
    /// Stop one or more services. With no args, stops all.
    Stop(ServiceArgs),
    /// Restart one or more services. With no args, restarts all.
    Restart(ServiceArgs),
    /// Validate and print the resolved configuration.
    Config(OutputOnlyArgs),
    /// Send a signal to one or more services. With no args, targets all.
    Kill(KillArgs),
    /// List running decompose environments.
    Ls(OutputOnlyArgs),
    /// Run a one-off command in a service's environment (env vars, working
    /// dir, env_file) without attaching to a running replica. Does not require
    /// a running daemon. Fire-and-forget: the command is not added to the
    /// supervised process list.
    Run(RunArgs),
    /// Execute a one-off command in the environment of a currently-running
    /// service. Requires the daemon to be running and at least one replica of
    /// SERVICE to be in the Running state. Otherwise behaves like `run`.
    Exec(ExecArgs),
    #[command(hide = true)]
    Daemon(DaemonArgs),
}

#[derive(Args, Debug, Clone)]
pub struct RunArgs {
    /// Override working directory for the command.
    #[arg(short = 'w', long = "workdir")]
    pub workdir: Option<PathBuf>,
    /// Extra environment variables (KEY=VALUE). Can be repeated. Override
    /// values from the service environment.
    #[arg(long = "env", value_name = "KEY=VALUE")]
    pub env: Vec<String>,
    /// Service whose environment to use.
    pub service: String,
    /// Command and arguments to execute. Everything after SERVICE is treated
    /// as the command.
    #[arg(trailing_var_arg = true, required = true, num_args = 1..)]
    pub command: Vec<String>,
}

#[derive(Args, Debug, Clone)]
pub struct ExecArgs {
    /// Override working directory for the command.
    #[arg(short = 'w', long = "workdir")]
    pub workdir: Option<PathBuf>,
    /// Extra environment variables (KEY=VALUE). Can be repeated.
    #[arg(long = "env", value_name = "KEY=VALUE")]
    pub env: Vec<String>,
    /// Service whose environment to attach to.
    pub service: String,
    /// Command and arguments to execute.
    #[arg(trailing_var_arg = true, required = true, num_args = 1..)]
    pub command: Vec<String>,
}

#[derive(Args, Debug, Clone)]
pub struct OutputOnlyArgs {
    #[command(flatten)]
    pub output: OutputArgs,
}

#[derive(Args, Debug, Clone)]
pub struct DownArgs {
    #[command(flatten)]
    pub output: OutputArgs,
    /// Override shutdown timeout in seconds for all processes.
    #[arg(short = 't', long = "timeout")]
    pub timeout: Option<u64>,
}

#[derive(Args, Debug, Clone)]
pub struct UpArgs {
    #[command(flatten)]
    pub output: OutputArgs,
    /// Start and return immediately.
    #[arg(short = 'd', long = "detach")]
    pub detach: bool,
    /// Wait until all services are healthy/started before returning (requires -d).
    #[arg(long = "wait")]
    pub wait: bool,
    /// Do not start dependency processes automatically.
    #[arg(long = "no-deps")]
    pub no_deps: bool,
    /// Remove processes not defined in the current config.
    #[arg(long = "remove-orphans")]
    pub remove_orphans: bool,
    /// Recreate every process regardless of whether its config hash changed.
    #[arg(long = "force-recreate", conflicts_with = "no_recreate")]
    pub force_recreate: bool,
    /// Keep existing processes even if their config hash differs.
    #[arg(long = "no-recreate")]
    pub no_recreate: bool,
    /// Create/register new/changed processes but don't start them.
    #[arg(long = "no-start")]
    pub no_start: bool,
    /// Start only these processes (and their dependencies, unless --no-deps).
    pub processes: Vec<String>,
}

#[derive(Args, Debug, Clone)]
pub struct ServiceArgs {
    #[command(flatten)]
    pub output: OutputArgs,
    /// Service(s) to operate on. If none, the operation applies to all services.
    pub services: Vec<String>,
}

#[derive(Args, Debug, Clone)]
pub struct LogsArgs {
    /// Follow log output.
    #[arg(short = 'f', long = "follow")]
    pub follow: bool,
    /// Number of lines to show from end of log.
    #[arg(short = 'n', long = "tail")]
    pub tail: Option<usize>,
    /// Disable paging (do not pipe output through $PAGER).
    #[arg(long = "no-pager")]
    pub no_pager: bool,
    /// Filter logs to specific process(es).
    pub processes: Vec<String>,
}

#[derive(Args, Debug, Clone)]
pub struct KillArgs {
    #[command(flatten)]
    pub output: OutputArgs,
    /// Signal to send (default: SIGKILL). Name (e.g. SIGTERM) or number (e.g. 15).
    #[arg(short = 's', long = "signal", default_value = "SIGKILL")]
    pub signal: String,
    /// Service(s) to kill. If none, kills all services.
    pub services: Vec<String>,
}

#[derive(Args, Debug, Clone)]
pub struct DaemonArgs {
    #[arg(long = "cwd")]
    pub cwd: PathBuf,
    #[arg(long = "config-file")]
    pub config_files: Vec<PathBuf>,
    #[arg(long = "instance")]
    pub instance: String,
    #[arg(long = "env-file")]
    pub env_files: Vec<PathBuf>,
    #[arg(long = "disable-dotenv")]
    pub disable_dotenv: bool,
    #[arg(long = "process")]
    pub processes: Vec<String>,
    #[arg(long = "no-deps")]
    pub no_deps: bool,
    /// PID to watch for orphan detection. When the PID exits and no IPC
    /// client has talked to the daemon for the configured grace period,
    /// the daemon self-terminates. Omitted in detached mode so the daemon
    /// survives its launching process by design.
    #[arg(long = "parent-pid")]
    pub parent_pid: Option<u32>,
}
