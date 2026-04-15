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

Config files: compose.yml, compose.yaml, decompose.yml, decompose.yaml
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
    Down(OutputOnlyArgs),
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
    #[command(hide = true)]
    Daemon(DaemonArgs),
}

#[derive(Args, Debug, Clone)]
pub struct OutputOnlyArgs {
    #[command(flatten)]
    pub output: OutputArgs,
}

#[derive(Args, Debug, Clone)]
pub struct UpArgs {
    #[command(flatten)]
    pub output: OutputArgs,
    /// Start and return immediately.
    #[arg(short = 'd', long = "detach")]
    pub detach: bool,
    /// Do not start dependency processes automatically.
    #[arg(long = "no-deps")]
    pub no_deps: bool,
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
}
