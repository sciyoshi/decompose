use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

use crate::output::OutputArgs;

#[derive(Parser, Debug)]
#[command(author, version, about)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Start the environment.
    Up(UpArgs),
    /// Stop the environment daemon and its managed processes.
    Down(GlobalArgs),
    /// Show process status.
    Ps(GlobalArgs),
    /// Reattach to a running environment's logs.
    Attach(GlobalArgs),
    /// View process logs.
    Logs(LogsArgs),
    /// Start one or more previously-stopped services. With no args, starts all.
    Start(ServiceArgs),
    /// Stop one or more services. With no args, stops all.
    Stop(ServiceArgs),
    /// Restart one or more services. With no args, restarts all.
    Restart(ServiceArgs),
    /// Process management operations (scale, etc.).
    Process {
        #[command(flatten)]
        global: GlobalArgs,
        #[command(subcommand)]
        command: ProcessCommand,
    },
    /// Port-related operations (not implemented in this rewrite yet).
    Ports {
        #[command(flatten)]
        global: GlobalArgs,
        #[command(subcommand)]
        command: PortsCommand,
    },
    #[command(hide = true)]
    Daemon(DaemonArgs),
}

#[derive(Args, Debug, Clone)]
pub struct GlobalArgs {
    /// Config file path(s). If omitted, auto-discovery is used. Can be repeated.
    #[arg(short = 'f', long = "file")]
    pub config_files: Vec<PathBuf>,
    /// Session/project name override for instance identity.
    #[arg(long = "session", alias = "project-name", env = "DECOMPOSE_SESSION")]
    pub session: Option<String>,
    /// Additional .env files to load.
    #[arg(short = 'e', long = "env-file")]
    pub env_files: Vec<PathBuf>,
    /// Disable automatic .env file loading.
    #[arg(long = "disable-dotenv")]
    pub disable_dotenv: bool,
    #[command(flatten)]
    pub output: OutputArgs,
}

#[derive(Args, Debug, Clone)]
pub struct UpArgs {
    #[command(flatten)]
    pub global: GlobalArgs,
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
    pub global: GlobalArgs,
    /// Service(s) to operate on. If none, the operation applies to all services.
    pub services: Vec<String>,
}

#[derive(Args, Debug, Clone)]
pub struct LogsArgs {
    #[command(flatten)]
    pub global: GlobalArgs,
    /// Follow log output.
    #[arg(long = "follow")]
    pub follow: bool,
    /// Number of lines to show from end of log.
    #[arg(short = 'n', long = "tail")]
    pub tail: Option<usize>,
    /// Filter logs to specific process(es).
    pub processes: Vec<String>,
}

#[derive(Subcommand, Debug, Clone)]
pub enum ProcessCommand {
    /// Scale a process to a given number of replicas.
    Scale {
        /// Process name to scale.
        process: String,
        /// Number of replicas.
        replicas: u16,
    },
}

#[derive(Subcommand, Debug, Clone)]
pub enum PortsCommand {
    List,
    Free,
    Release {
        service_name: Option<String>,
    },
    Reserve {
        port: u16,
        service_name: Option<String>,
    },
    Inspect {
        service_name: String,
    },
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
