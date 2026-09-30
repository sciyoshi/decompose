//! Decompose: a process orchestrator for local development with
//! Docker-Compose-compatible config and CLI surface.
//!
//! The crate is split into a thin client and a long-running daemon. The
//! `decompose` binary always runs as the client; on `up` it spawns a detached
//! daemon process per project (identified by a SHA-256 of the config
//! directory plus file set, or by `--session NAME`) and then talks to it
//! over a local socket.
//!
//! # Module map
//!
//! - [`cli`]      — `clap` argument definitions for every subcommand.
//! - [`config`]   — YAML parsing, merge/overlay, `.env` loading, `${VAR}`
//!   interpolation, and validation.
//! - [`daemon`]   — supervisor loop, IPC server, process lifecycle, signal
//!   handling, reload/diff, health-probe scheduling.
//! - [`ipc`]      — request/response wire types and a JSON-over-local-socket
//!   transport used by both halves.
//! - [`model`]    — runtime types shared by the daemon and its clients
//!   (`ProcessInstanceSpec`, `ProcessRuntime`, `ProcessSnapshot`,
//!   `HealthProbe`, …).
//! - [`output`]   — JSON / text formatting and coordinated output writers.
//! - [`paths`]    — XDG path management and instance-ID hashing.
//! - [`tui`]      — the optional interactive terminal UI built on `ratatui`.
//! - [`tuning`]   — env-var-overridable timing knobs (supervisor tick, IPC
//!   timeout, orphan grace period).
//! - [`completion`] — shell completion script generator.
//! - [`health_probes`] — exec/HTTP probe execution for readiness and
//!   liveness checks.
//!
//! [`run_cli`] is the single entry point used by `main.rs`. See `CLAUDE.md`
//! for the full design notes and project conventions.

pub mod cli;
pub mod completion;
pub mod config;
pub mod daemon;
pub mod diagnostic;
pub mod health_probes;
mod hooks;
pub mod ipc;
mod logs;
pub mod model;
pub mod output;
pub mod output_model;
pub mod paths;
#[cfg(unix)]
mod process_table;
mod shutdown;
pub mod tui;
pub mod tuning;
mod wait_progress;

use std::env;
use std::io::Write as _;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::signal::ctrl_c;
use tokio::sync::watch;
use tokio::time::sleep;

use crate::cli::{Cli, Commands, ExecArgs, KillArgs, LogsArgs, RunArgs, ServiceArgs, UpArgs};
use crate::config::{build_process_instances, load_project, resolve_config_paths};
use crate::daemon::{run_daemon, spawn_daemon_process};
use crate::ipc::{Request, Response, send_request};
use crate::output::{OutputMode, print_json, styled, unified_state, use_color};
use crate::output_model::{
    Acknowledgment, Changes, Daemon, DaemonAction, DaemonState, OperationResult, Outcome,
    Readiness, ServiceOutcome, ServiceResult,
};
use crate::paths::{build_instance_id, runtime_dir, runtime_paths_for};
use crate::wait_progress::WaitProgress;

/// A one-off child's status, returned to library callers without terminating
/// their process. The binary preserves this status without a CLI diagnostic.
#[derive(Debug)]
pub struct ChildExitStatus(pub i32);
impl std::fmt::Display for ChildExitStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "child exited with status {}", self.0)
    }
}
impl std::error::Error for ChildExitStatus {}

/// Global config flags that live on the top-level `Cli` struct.
#[derive(Debug, Clone)]
pub struct GlobalConfig {
    pub config_files: Vec<PathBuf>,
    pub session: Option<String>,
    pub env_files: Vec<PathBuf>,
    pub disable_dotenv: bool,
}

/// Parse and execute an invocation, rendering terminal failures exactly once.
/// All decompose output uses the supplied writers. Child streams remain
/// passthrough; `run_cli` remains available to callers handling errors.
pub async fn run_cli_from(
    args: impl IntoIterator<Item = std::ffi::OsString>,
    stdout: &mut impl std::io::Write,
    stderr: &mut impl std::io::Write,
) -> u8 {
    let args: Vec<_> = args.into_iter().collect();
    let mut mode = cli::diagnostic_mode(&args);
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(error) => {
            use clap::error::ErrorKind;
            if matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) {
                return if write!(stdout, "{error}").is_ok() {
                    0
                } else {
                    1
                };
            }
            let text = error.to_string();
            let (summary, usage) = text.split_once("\n\n").unwrap_or((&text, ""));
            let mut diagnostic = diagnostic::Diagnostic::error(
                "invalid_arguments",
                summary
                    .trim()
                    .strip_prefix("error: ")
                    .unwrap_or(summary.trim()),
            );
            if !usage.is_empty() {
                diagnostic.usage = Some(usage.trim().into());
            }
            let _ = diagnostic.write(mode, stderr);
            return 2;
        }
    };
    if matches!(&cli.command, Commands::Daemon(_)) {
        mode = OutputMode::Json;
    }
    let read_only = matches!(
        &cli.command,
        Commands::Ps(_)
            | Commands::Ls(_)
            | Commands::Logs(_)
            | Commands::Attach(_)
            | Commands::Config(_)
            | Commands::Completion(_)
    );
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    let invocation = output::STDOUT.scope(
        sender,
        diagnostic::WARNINGS.scope(std::cell::RefCell::new(Vec::new()), async {
            let result = dispatch_cli(cli).await;
            let warnings = diagnostic::WARNINGS.with(|warnings| warnings.take());
            (result, warnings)
        }),
    );
    tokio::pin!(invocation);
    let mut output_error = None;
    let (mut result, warnings) = loop {
        tokio::select! {
            result = &mut invocation => break result,
            Some(bytes) = receiver.recv() => {
                if output_error.is_none() && let Err(error) = stdout.write_all(&bytes) {
                    output_error = Some(error);
                    receiver.close();
                }
            }
        }
    };
    while let Ok(bytes) = receiver.try_recv() {
        if output_error.is_none()
            && let Err(error) = stdout.write_all(&bytes)
        {
            output_error = Some(error);
        }
    }
    if result.is_ok()
        && let Some(error) = output_error
    {
        if read_only && error.kind() == std::io::ErrorKind::BrokenPipe {
            return 0;
        }
        let mut diagnostic = diagnostic::Diagnostic::from_error(
            &anyhow::Error::new(error).context("failed to write output"),
        );
        diagnostic.code = "output_write_failed".into();
        result = Err(diagnostic.into());
    }
    for warning in warnings {
        let _ = warning.write(mode, stderr);
    }
    match result {
        Ok(()) => 0,
        Err(error) => {
            if let Some(status) = error.downcast_ref::<ChildExitStatus>() {
                return status.0 as u8;
            }
            if read_only
                && error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::BrokenPipe)
            {
                return 0;
            }
            let diagnostic = diagnostic::Diagnostic::from_error(&error);
            let code = if matches!(
                diagnostic.code.as_str(),
                "invalid_arguments" | "unknown_signal" | "invalid_environment_override"
            ) {
                2
            } else if diagnostic.code == "interrupted" {
                130
            } else {
                1
            };
            let _ = diagnostic.write(mode, stderr);
            code
        }
    }
}

/// Execute the process arguments, returning errors to the library caller.
pub async fn run_cli() -> Result<()> {
    dispatch_cli(Cli::try_parse_from(env::args_os())?).await
}

async fn dispatch_cli(mut cli: Cli) -> Result<()> {
    let output = cli.output.clone();
    match &mut cli.command {
        Commands::Up(args) => args.output = output.clone(),
        Commands::Down(args) => args.output = output.clone(),
        Commands::Ps(args)
        | Commands::Attach(args)
        | Commands::Config(args)
        | Commands::Ls(args) => args.output = output.clone(),
        Commands::Start(args) | Commands::Stop(args) | Commands::Restart(args) => {
            args.output = output.clone()
        }
        Commands::Kill(args) => args.output = output.clone(),
        _ => {}
    }
    if output.json
        && matches!(
            &cli.command,
            Commands::Tui | Commands::Up(UpArgs { tui: true, .. })
        )
    {
        return Err(diagnostic::Diagnostic::error(
            "invalid_arguments",
            "--json is incompatible with the TUI",
        )
        .into());
    }

    let global = GlobalConfig {
        config_files: cli.config_files,
        session: cli.session,
        env_files: cli.env_files,
        disable_dotenv: cli.disable_dotenv,
    };

    match cli.command {
        Commands::Up(args) => run_up(global, args).await,
        Commands::Down(args) => run_down(global, args.output.resolve(), args.timeout).await,
        Commands::Ps(args) => run_ps(global, args.output.resolve()).await,
        Commands::Attach(args) => run_attach(global, args.output.resolve()).await,
        Commands::Tui => run_tui(global).await,
        Commands::Logs(args) => run_logs(global, args, output.resolve()).await,
        Commands::Start(args) => run_service_command(global, args, ServiceOp::Start).await,
        Commands::Stop(args) => run_service_command(global, args, ServiceOp::Stop).await,
        Commands::Restart(args) => run_service_command(global, args, ServiceOp::Restart).await,
        Commands::Config(args) => run_config(global, args.output.resolve()).await,
        Commands::Kill(args) => run_kill(global, args).await,
        Commands::Ls(args) => run_ls(args.output.resolve()).await,
        Commands::Run(args) => run_run(global, args).await,
        Commands::Exec(args) => run_exec(global, args).await,
        Commands::Completion(args) => crate::completion::run_completion(args.shell),
        Commands::Daemon(args) => run_daemon(args).await,
    }
}

/// Build the environment/working_dir for a service from the on-disk config,
/// exactly the way the daemon does when spawning it.
fn resolve_service_context(
    global: &GlobalConfig,
    service: &str,
) -> Result<(PathBuf, std::collections::BTreeMap<String, String>)> {
    let cwd = env::current_dir().context("failed to read current directory")?;
    let config_files = resolve_config_paths(&global.config_files, &cwd)?;
    let config_dir = config_files[0].parent().unwrap_or(&cwd).to_path_buf();
    let loaded = load_project(&config_files, &global.env_files, global.disable_dotenv)?;
    let cfg = loaded.config;
    let dotenv = loaded.dotenv;
    if !cfg.processes.contains_key(service) {
        let known: Vec<&str> = cfg.processes.keys().map(|k| k.as_str()).collect();
        bail!(
            "unknown service: {service:?} (known services: {})",
            known.join(", ")
        );
    }
    let instances = build_process_instances(&cfg, &config_dir, &dotenv);
    crate::config::validate_resolved_hooks(&instances)?;
    // Pick the first replica (or the bare service name when replicas == 1).
    let (_, runtime) = instances
        .iter()
        .find(|(_, r)| r.spec.base_name == service)
        .ok_or_else(|| anyhow::anyhow!("service {service:?} has no replicas"))?;
    Ok((
        runtime.spec.working_dir.clone(),
        runtime.spec.environment.clone(),
    ))
}

/// Parse a `-e KEY=VALUE` override. Accepts `KEY=VALUE`; a bare `KEY` pulls
/// the value from the current process environment (matches `docker compose
/// run -e KEY` semantics). Returns an error for empty keys or leading-`=`.
fn parse_env_override(raw: &str) -> Result<(String, String)> {
    match raw.split_once('=') {
        Some(("", _)) => Err(diagnostic::Diagnostic::error(
            "invalid_environment_override",
            format!("invalid --env entry {raw:?}: empty key"),
        )
        .into()),
        Some((k, v)) => Ok((k.to_string(), v.to_string())),
        None => {
            if raw.is_empty() {
                return Err(diagnostic::Diagnostic::error(
                    "invalid_environment_override",
                    "invalid --env entry: empty string",
                )
                .into());
            }
            let v = env::var(raw).unwrap_or_default();
            Ok((raw.to_string(), v))
        }
    }
}

/// Spawn CMD... with the given cwd and environment, inheriting stdio so
/// interactive commands (psql, bash) work. Returns the child's exit code
/// (128 + signal on Unix signal termination).
fn spawn_one_off(
    cwd: &std::path::Path,
    env_vars: &std::collections::BTreeMap<String, String>,
    command: &[String],
) -> Result<i32> {
    let (program, args) = command.split_first().expect("clap guarantees non-empty");
    let mut cmd = std::process::Command::new(program);
    cmd.args(args);
    cmd.current_dir(cwd);
    // Start from a clean slate then overlay the service env so we don't leak
    // unrelated caller environment into the child (matching how the daemon
    // spawns services).
    cmd.env_clear();
    for (k, v) in env_vars {
        cmd.env(k, v);
    }
    // stdin/stdout/stderr default to inherit, which is what we want.
    let status = cmd
        .status()
        .with_context(|| format!("failed to spawn {program:?}"))?;
    if let Some(code) = status.code() {
        return Ok(code);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            return Ok(128 + sig);
        }
    }
    Ok(1)
}

async fn run_run(global: GlobalConfig, args: RunArgs) -> Result<()> {
    let (cwd, mut env_vars) = resolve_service_context(&global, &args.service)?;
    for raw in &args.env {
        let (k, v) = parse_env_override(raw)?;
        env_vars.insert(k, v);
    }
    let workdir = args.workdir.map(|p| {
        if p.is_absolute() {
            p
        } else {
            env::current_dir().map(|d| d.join(&p)).unwrap_or(p)
        }
    });
    let final_cwd = workdir.unwrap_or(cwd);
    let code = spawn_one_off(&final_cwd, &env_vars, &args.command)?;
    if code != 0 {
        return Err(ChildExitStatus(code).into());
    }
    Ok(())
}

async fn run_exec(global: GlobalConfig, args: ExecArgs) -> Result<()> {
    // `exec` requires a running service. Preflight against the daemon before
    // doing any local work so users get a clear "service not running" error.
    let paths = resolve_runtime_paths(&global.config_files, global.session.as_deref())?;

    let response = match send_request(
        &paths,
        Request::ServiceRunState {
            name: args.service.clone(),
        },
    )
    .await
    {
        Ok(resp) => resp,
        Err(err) if is_no_daemon_error(&err, &paths) => {
            bail!(
                "no running environment for this project — start one with `decompose up` (or use `decompose run` for a one-off command)"
            );
        }
        Err(err) => return Err(err),
    };

    match response {
        Response::ServiceRunState { known, any_running } => {
            if !known {
                bail!("unknown service: {:?}", args.service);
            }
            if !any_running {
                bail!(
                    "service {:?} is not running — start it with `decompose start {}` (or use `decompose run` for a one-off command)",
                    args.service,
                    args.service
                );
            }
        }
        Response::Error {
            message,
            diagnostic,
        } => return Err(Response::into_error(message, diagnostic)),
        _ => bail!("unexpected response from daemon"),
    }

    let (cwd, mut env_vars) = resolve_service_context(&global, &args.service)?;
    for raw in &args.env {
        let (k, v) = parse_env_override(raw)?;
        env_vars.insert(k, v);
    }
    let workdir = args.workdir.map(|p| {
        if p.is_absolute() {
            p
        } else {
            env::current_dir().map(|d| d.join(&p)).unwrap_or(p)
        }
    });
    let final_cwd = workdir.unwrap_or(cwd);
    let code = spawn_one_off(&final_cwd, &env_vars, &args.command)?;
    if code != 0 {
        return Err(ChildExitStatus(code).into());
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
enum ServiceOp {
    Start,
    Stop,
    Restart,
}

async fn run_up(global: GlobalConfig, args: UpArgs) -> Result<()> {
    let output_mode = args.output.resolve();
    // `--tui` means "start services, then hand off to the TUI". Like `-d`,
    // the caller is no longer tethered to the daemon while the TUI runs,
    // so we treat it as detached for daemon-lifetime purposes (no
    // parent_pid → daemon outlives the TUI process) and skip the log
    // stream. The TUI handles its own Ctrl-C.
    let attached = !args.detach && !args.tui;
    let ctrl_c_task = if attached {
        let mut signals = shutdown::Signals::new()?;
        Some(tokio::spawn(async move {
            signals.recv().await;
        }))
    } else {
        None
    };

    let UpContext {
        cwd,
        config_files,
        instance,
        paths,
    } = resolve_up_context(&global)?;

    let wait_set = if args.wait {
        let loaded = load_project(&config_files, &global.env_files, global.disable_dotenv)?;
        Some(if args.processes.is_empty() {
            loaded.config.processes.keys().cloned().collect()
        } else {
            crate::config::collect_process_subset(&loaded.config, &args.processes, !args.no_deps)?
        })
    } else {
        None
    };
    // Establish the cursor before starting/reloading services so fast startup
    // events are retained, while previous attempts on a live daemon stay hidden.
    let mut progress = if args.wait && output_mode == OutputMode::Table {
        let progress = WaitProgress::new(&paths).await?;
        Some(progress)
    } else {
        None
    };
    let (pid, state, got_ctrl_c, reload_summary) = ensure_daemon_running(
        &global,
        &args,
        &cwd,
        &config_files,
        &instance,
        &paths,
        output_mode,
        ctrl_c_task.as_ref(),
        attached,
    )
    .await?;

    // Orphan removal is folded into the Reload request on the already-running
    // branch. On the freshly-spawned daemon branch there are no orphans yet —
    // the daemon was just initialised from the current config — so a separate
    // RemoveOrphans call here would be a no-op. The standalone
    // Request::RemoveOrphans variant is still used by other code paths.

    if let Some(selected) = &wait_set {
        let result = wait_for_services_ready(&paths, selected, progress.as_mut()).await;
        if let Some(progress) = progress.as_mut() {
            progress.finish(result.is_ok())?;
        }
        result?;
    }
    let mut acknowledgment = if let Some(result) = reload_summary {
        result
    } else {
        let processes = match send_request(&paths, Request::Ps).await? {
            Response::Ps { processes, .. } => processes,
            _ => bail!("unexpected response to ps"),
        };
        let added = processes
            .iter()
            .map(|p| p.base.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        Acknowledgment {
            outcome: if args.no_start {
                Outcome::Completed
            } else {
                Outcome::Accepted
            },
            services: processes
                .into_iter()
                .filter(|p| p.state != "disabled" && (args.no_start || p.state != "not_started"))
                .map(|p| ServiceResult {
                    name: p.name,
                    outcome: if args.no_start {
                        ServiceOutcome::Registered
                    } else {
                        ServiceOutcome::StartRequested
                    },
                })
                .collect(),
            changes: Some(Changes {
                added,
                ..Default::default()
            }),
        }
    };
    if args.wait {
        acknowledgment.outcome = Outcome::Completed;
        for service in &mut acknowledgment.services {
            service.outcome = ServiceOutcome::Ready;
        }
    }
    acknowledgment.services.sort_by(|a, b| a.name.cmp(&b.name));
    let result = OperationResult {
        schema_version: "1.0",
        operation: "up".into(),
        daemon: Daemon {
            state: DaemonState::Running,
            pid: Some(pid),
            instance,
        },
        acknowledgment,
        daemon_action: Some(if state == "started" {
            DaemonAction::Started
        } else {
            DaemonAction::Reused
        }),
        readiness: Some(if args.wait {
            Readiness::Satisfied
        } else {
            Readiness::NotRequested
        }),
        signal: None,
    };
    if !attached {
        emit_operation(output_mode, &result)?;
    } else if state == "started" {
        emit_session_event(
            output_mode,
            output_model::SessionEvent::DaemonStarted {
                daemon: result.daemon.clone(),
            },
        )?;
    } else {
        emit_session_event(
            output_mode,
            output_model::SessionEvent::ConfigurationReloaded {
                changes: result.acknowledgment.changes.clone().unwrap_or_default(),
            },
        )?;
    }

    if !attached {
        if args.tui {
            return tui::run(paths).await;
        }
        return Ok(());
    }
    if got_ctrl_c {
        if state == "started" {
            stop_environment(&paths, pid, None).await?;
        } else {
            emit_detach(output_mode)?;
        }
        return Ok(());
    }

    stream_logs_until_ctrl_c(
        &paths,
        output_mode,
        state == "already_running",
        ctrl_c_task,
        pid,
    )
    .await
}

/// Resolved paths + config inputs used across the `up` flow.
struct UpContext {
    cwd: PathBuf,
    config_files: Vec<PathBuf>,
    instance: String,
    paths: crate::model::RuntimePaths,
}

fn resolve_up_context(global: &GlobalConfig) -> Result<UpContext> {
    let cwd = env::current_dir().context("failed to read current directory")?;
    let config_files = resolve_config_paths(&global.config_files, &cwd)?;
    let config_dir = config_files[0].parent().unwrap_or(&cwd).to_path_buf();
    let instance = build_instance_id(global.session.as_deref(), &config_dir, &config_files);
    let paths = runtime_paths_for(&instance)?;
    Ok(UpContext {
        cwd: config_dir,
        config_files,
        instance,
        paths,
    })
}

/// Ensure a daemon is running for this project: either reload/start against
/// an existing daemon, or spawn a fresh one. Returns the daemon PID, the
/// textual state ("started" or "already_running"), a flag indicating
/// whether the user hit Ctrl-C while we were waiting for the new daemon,
/// and (for the already-running branch) the parsed reload summary.
#[allow(clippy::too_many_arguments)]
async fn ensure_daemon_running(
    global: &GlobalConfig,
    args: &UpArgs,
    cwd: &std::path::Path,
    config_files: &[PathBuf],
    instance: &str,
    paths: &crate::model::RuntimePaths,
    output_mode: OutputMode,
    ctrl_c_task: Option<&tokio::task::JoinHandle<()>>,
    attached: bool,
) -> Result<(u32, &'static str, bool, Option<Acknowledgment>)> {
    let response = send_request(paths, Request::Ping).await;
    if let Err(error) = &response
        && !is_no_daemon_error(error, paths)
    {
        return Err(response.unwrap_err()).context("daemon discovery failed");
    }
    if response.is_ok() && !matches!(&response, Ok(Response::Pong { .. })) {
        return Err(diagnostic::Diagnostic::error(
            "ipc_unexpected_response",
            "unexpected response to ping",
        )
        .into());
    }
    if let Ok(Response::Pong {
        pid, shutting_down, ..
    }) = response
    {
        if shutting_down {
            // Reload/Start would race against the supervisor's tear-down loop:
            // newly-started services would be immediately stopped again, and
            // `ps` would show an empty environment. Make the user wait for
            // the previous environment to finish exiting first.
            bail!(
                "decompose is stopping for this project — wait for it to finish, then run `decompose up` again"
            );
        }
        preflight_validate_config(global, config_files, &args.processes)
            .context("failed to reload configuration")?;
        let summary = reload_and_start_existing_daemon(args, paths, output_mode).await?;
        Ok((pid, "already_running", false, summary))
    } else {
        // Clean up stale socket/pid from a previously killed daemon so the
        // new daemon can bind the socket without interference.
        cleanup_stale_files(paths);
        preflight_validate_config(global, config_files, &args.processes)?;
        // Attached `up` stays tethered to its daemon: if the user Ctrl-C's
        // out or the terminal is closed, the daemon should auto-exit rather
        // than leak. Detached `up -d` explicitly opts into a daemon that
        // outlives its launcher, so we pass `None` there.
        let parent_pid = if attached {
            Some(std::process::id())
        } else {
            None
        };
        spawn_daemon_process(
            cwd,
            config_files,
            instance,
            paths,
            &global.env_files,
            global.disable_dotenv,
            &args.processes,
            args.no_deps,
            args.no_start,
            parent_pid,
        )?;
        let (pid, got_ctrl_c) = wait_for_daemon_ready(paths, ctrl_c_task).await?;
        Ok((pid, "started", got_ctrl_c, None))
    }
}

async fn reload_and_start_existing_daemon(
    args: &UpArgs,
    paths: &crate::model::RuntimePaths,
    _output_mode: OutputMode,
) -> Result<Option<Acknowledgment>> {
    let reload_resp = send_request(
        paths,
        Request::Reload {
            force_recreate: args.force_recreate,
            no_recreate: args.no_recreate,
            remove_orphans: args.remove_orphans,
            no_start: args.no_start,
        },
    )
    .await
    .context("failed to reload daemon config")?;
    let mut result = expect_operation(reload_resp)?;
    if let Some(changes) = &result.changes
        && !changes.orphans.is_empty()
    {
        diagnostic::warning(
            "orphan_services",
            format!(
                "orphan services left running: {}",
                changes.orphans.join(", ")
            ),
        );
    }
    if args.no_start
        && let Response::Ps { processes, .. } = send_request(paths, Request::Ps).await?
    {
        result.services = processes
            .into_iter()
            .filter(|p| {
                result
                    .changes
                    .as_ref()
                    .is_some_and(|c| c.added.contains(&p.base) || c.changed.contains(&p.base))
            })
            .map(|p| ServiceResult {
                name: p.name,
                outcome: ServiceOutcome::Registered,
            })
            .collect();
    }
    // Start is idempotent on already-running processes and picks up any
    // newly-added ones that reload inserted as Pending. Skipped under
    // --no-start: the user asked to register-but-not-launch.
    if !args.no_start {
        let start_resp = send_request(
            paths,
            Request::Start {
                services: args.processes.clone(),
            },
        )
        .await
        .context("failed to start services on running daemon")?;
        let start = expect_operation(start_resp)?;
        result.services = start.services;
        result.outcome = start.outcome;
    }
    Ok(Some(result))
}

/// Validate the merged config and the requested service names before
/// spawning the daemon, so users see structured errors (dependency cycles,
/// unknown services) instead of a generic "daemon did not become ready"
/// timeout.
fn preflight_validate_config(
    global: &GlobalConfig,
    config_files: &[PathBuf],
    processes: &[String],
) -> Result<()> {
    let preflight = load_project(config_files, &global.env_files, global.disable_dotenv)
        .context("config validation failed before starting daemon")?
        .config;
    if processes.is_empty() {
        return Ok(());
    }
    let known: std::collections::HashSet<&str> =
        preflight.processes.keys().map(|k| k.as_str()).collect();
    let unknown: Vec<&str> = processes
        .iter()
        .filter(|p| !known.contains(p.as_str()))
        .map(|p| p.as_str())
        .collect();
    if !unknown.is_empty() {
        bail!("unknown service(s): {}", unknown.join(", "));
    }
    Ok(())
}

/// Poll the freshly-spawned daemon until it responds to Ping. Returns the
/// PID and whether the Ctrl-C listener fired while we were waiting. Bails
/// if the daemon never becomes ready within the poll budget.
async fn wait_for_daemon_ready(
    paths: &crate::model::RuntimePaths,
    ctrl_c_task: Option<&tokio::task::JoinHandle<()>>,
) -> Result<(u32, bool)> {
    let mut got_ctrl_c = false;
    for _ in 0..80 {
        if let Ok(Response::Pong { pid, .. }) = send_request(paths, Request::Ping).await {
            return Ok((pid, got_ctrl_c));
        }
        if let Some(task) = ctrl_c_task
            && task.is_finished()
        {
            got_ctrl_c = true;
        }
        sleep(Duration::from_millis(50)).await;
    }
    Err(startup_diagnostic(paths).into())
}

fn startup_diagnostic(paths: &crate::model::RuntimePaths) -> diagnostic::Diagnostic {
    use std::io::{Read, Seek, SeekFrom};
    let mut excerpt = Vec::new();
    let mut truncated = false;
    if let Ok(mut file) = std::fs::File::open(&paths.daemon_log) {
        let len = file.metadata().map(|m| m.len()).unwrap_or(0);
        let start = len.saturating_sub(16 * 1024);
        truncated = start > 0;
        if file.seek(SeekFrom::Start(start)).is_ok() {
            let mut bytes = Vec::new();
            if file.take(16 * 1024).read_to_end(&mut bytes).is_ok() {
                excerpt = String::from_utf8_lossy(&bytes)
                    .lines()
                    .map(str::to_owned)
                    .collect();
                if start > 0 && !excerpt.is_empty() {
                    excerpt.remove(0);
                }
                if excerpt.len() > 20 {
                    excerpt.drain(..excerpt.len() - 20);
                    truncated = true;
                }
            }
        }
    }
    let mut error = excerpt
        .iter()
        .rev()
        .find_map(|line| {
            serde_json::from_str::<diagnostic::Diagnostic>(line)
                .ok()
                .filter(|d| d.severity == "error")
        })
        .unwrap_or_else(|| {
            diagnostic::Diagnostic::error("daemon_start_timeout", "daemon did not become ready")
        });
    error.details = Some(diagnostic::DiagnosticDetails::Startup {
        daemon_log: paths.daemon_log.clone(),
        log_excerpt: excerpt,
        excerpt_truncated: truncated,
    });
    error
}

/// Stream service logs until the Ctrl-C task fires, then stop the log
/// streamer and emit the "detached" marker. Consumes `ctrl_c_task`.
async fn stream_logs_until_ctrl_c(
    paths: &crate::model::RuntimePaths,
    output_mode: OutputMode,
    start_at_end: bool,
    ctrl_c_task: Option<tokio::task::JoinHandle<()>>,
    pid: u32,
) -> Result<()> {
    let (log_stop_tx, log_stop_rx) = watch::channel(false);
    let mut log_handle = output::spawn(stream_daemon_logs(
        paths.daemon_log.clone(),
        log_stop_rx,
        start_at_end,
        output_mode,
    ));
    if start_at_end {
        emit_attach(output_mode)?;
    } else {
        emit_session_event(
            output_mode,
            output_model::SessionEvent::Attached {
                ownership: output_model::Ownership::Owner,
            },
        )?;
    }
    let outcome = if let Some(mut task) = ctrl_c_task {
        loop {
            tokio::select! {
                result = &mut log_handle => {
                    task.abort();
                    if !start_at_end { stop_environment(paths, pid, None).await?; }
                    return result?;
                }
                result = &mut task => {
                    result.context("failed waiting for shutdown signal")?;
                    break if start_at_end { Ok(()) } else { stop_environment(paths, pid, None).await };
                }
                _ = sleep(Duration::from_millis(100)) => {
                    if !daemon_pid_matches(paths, pid) {
                        task.abort();
                        break read_shutdown_receipt(paths, pid);
                    }
                }
            }
        }
    } else {
        Ok(())
    };
    let _ = log_stop_tx.send(true);
    let log_outcome = log_handle.await;
    outcome?;
    log_outcome??;
    if start_at_end {
        emit_detach(output_mode)?;
    } else {
        emit_session_event(
            output_mode,
            output_model::SessionEvent::EnvironmentStopped {
                ownership: output_model::Ownership::Owner,
            },
        )?;
    }
    Ok(())
}

async fn run_down(
    global: GlobalConfig,
    output_mode: OutputMode,
    timeout: Option<u64>,
) -> Result<()> {
    let paths = resolve_runtime_paths(&global.config_files, global.session.as_deref())?;

    let mut acknowledgment = Acknowledgment {
        outcome: Outcome::Completed,
        services: Vec::new(),
        changes: None,
    };
    let pid = match send_request(&paths, Request::Ping).await {
        Ok(Response::Pong { pid, .. }) => pid,
        Ok(_) => bail!("unexpected response from daemon"),
        Err(err) if is_no_daemon_error(&err, &paths) => {
            if let Ok(contents) = std::fs::read_to_string(&paths.pid)
                && let Ok(pid) = contents.trim().parse::<u32>()
            {
                if daemon_pid_matches(&paths, pid) {
                    return Err(err).context("daemon is alive but unreachable");
                }
                read_shutdown_receipt(&paths, pid)?;
            }
            acknowledgment.outcome = Outcome::Unchanged;
            return emit_down(output_mode, &paths, acknowledgment);
        }
        Err(err) => return Err(err),
    };
    if let Response::Ps { processes, .. } = send_request(&paths, Request::Ps).await? {
        acknowledgment.services = processes
            .into_iter()
            .map(|p| ServiceResult {
                name: p.name,
                outcome: if matches!(
                    p.state.as_str(),
                    "not_started"
                        | "disabled"
                        | "stopped"
                        | "exited"
                        | "failed"
                        | "failed_to_start"
                ) {
                    ServiceOutcome::AlreadyStopped
                } else {
                    ServiceOutcome::Stopped
                },
            })
            .collect();
        acknowledgment.services.sort_by(|a, b| a.name.cmp(&b.name));
    }
    stop_environment(&paths, pid, timeout).await?;
    emit_down(output_mode, &paths, acknowledgment)
}

fn emit_down(
    mode: OutputMode,
    paths: &crate::model::RuntimePaths,
    acknowledgment: Acknowledgment,
) -> Result<()> {
    emit_operation(
        mode,
        &OperationResult {
            schema_version: "1.0",
            operation: "down".into(),
            daemon: Daemon {
                state: DaemonState::NotRunning,
                pid: None,
                instance: paths
                    .socket
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
            },
            acknowledgment,
            daemon_action: None,
            readiness: None,
            signal: None,
        },
    )
}

async fn shutdown_budget(
    paths: &crate::model::RuntimePaths,
    timeout: Option<u64>,
) -> Result<Duration> {
    match send_request(
        paths,
        Request::ShutdownBudget {
            timeout_seconds: timeout,
        },
    )
    .await?
    {
        Response::ShutdownBudget { seconds } => Ok(Duration::from_secs(seconds)),
        Response::Error {
            message,
            diagnostic,
        } => Err(Response::into_error(message, diagnostic)),
        _ => bail!("unexpected shutdown budget response"),
    }
}

pub(crate) async fn stop_environment(
    paths: &crate::model::RuntimePaths,
    pid: u32,
    timeout: Option<u64>,
) -> Result<()> {
    let mut signals = shutdown::Signals::new()?;
    let budget = shutdown_budget(paths, timeout).await?;
    expect_ack(
        send_request(
            paths,
            Request::Down {
                timeout_seconds: timeout,
            },
        )
        .await?,
    )?;
    let wait = wait_for_daemon_stop(paths, pid, budget);
    tokio::pin!(wait);
    loop {
        tokio::select! {
            result = &mut wait => return result,
            _ = signals.recv() => {
                expect_ack(send_request(paths, Request::ForceDown).await?)?;
            }
        }
    }
}

async fn run_ps(global: GlobalConfig, output_mode: OutputMode) -> Result<()> {
    let paths = resolve_runtime_paths(&global.config_files, global.session.as_deref())?;
    let response = match send_request(&paths, Request::Ps).await {
        Ok(response) => response,
        Err(err) if is_no_daemon_error(&err, &paths) => {
            emit_ps(
                output_mode,
                output_model::Daemon {
                    state: output_model::DaemonState::NotRunning,
                    pid: None,
                    instance: paths
                        .socket
                        .file_stem()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned(),
                },
                &[],
            )?;
            return Ok(());
        }
        Err(err) => return Err(err),
    };

    match response {
        Response::Ps {
            pid,
            instance,
            mut processes,
            shutting_down,
        } => {
            processes.sort_by(|a, b| a.name.cmp(&b.name));
            emit_ps(
                output_mode,
                output_model::Daemon {
                    state: if shutting_down {
                        output_model::DaemonState::Stopping
                    } else {
                        output_model::DaemonState::Running
                    },
                    pid: Some(pid),
                    instance,
                },
                &processes,
            )?;
            Ok(())
        }
        Response::Error {
            message,
            diagnostic,
        } => Err(Response::into_error(message, diagnostic)),
        _ => bail!("unexpected response from daemon"),
    }
}

async fn run_tui(global: GlobalConfig) -> Result<()> {
    let paths = resolve_runtime_paths(&global.config_files, global.session.as_deref())?;
    match send_request(&paths, Request::Ping).await {
        Ok(Response::Pong { .. }) => {}
        _ => bail!(
            "no running environment for this project — start one with `decompose up -d` first"
        ),
    }
    tui::run(paths).await
}

async fn run_attach(global: GlobalConfig, output_mode: OutputMode) -> Result<()> {
    let paths = resolve_runtime_paths(&global.config_files, global.session.as_deref())?;

    match send_request(&paths, Request::Ping).await {
        Ok(Response::Pong { .. }) => {}
        _ => bail!("no running environment for this project — start one with `decompose up`"),
    };

    emit_attach(output_mode)?;

    let (log_stop_tx, log_stop_rx) = watch::channel(false);
    let mut log_handle = output::spawn(stream_daemon_logs(
        paths.daemon_log.clone(),
        log_stop_rx,
        false,
        output_mode,
    ));

    tokio::select! {
        result = &mut log_handle => return result?,
        signal = ctrl_c() => { signal.context("failed to listen for Ctrl-C")?; }
    }

    let _ = log_stop_tx.send(true);
    log_handle.await??;
    emit_detach(output_mode)?;
    Ok(())
}

async fn run_logs(global: GlobalConfig, args: LogsArgs, mode: OutputMode) -> Result<()> {
    let paths = resolve_runtime_paths(&global.config_files, global.session.as_deref())?;

    match send_request(&paths, Request::Ping).await {
        Ok(Response::Pong { .. }) => {}
        _ => bail!("no running environment for this project — start one with `decompose up`"),
    };

    let mut reader = crate::logs::Reader::default();
    let backlog = reader
        .poll_records(&paths.daemon_log, &args.processes, args.tail)
        .await?;
    if args.follow {
        for line in &backlog {
            line.write(mode, args.processes.len() == 1)?;
        }
        let _ = std::io::stdout().flush();
        let (log_stop_tx, log_stop_rx) = watch::channel(false);
        let mut log_handle = output::spawn(stream_filtered_logs(
            paths.clone(),
            log_stop_rx,
            args.processes,
            reader,
            mode,
        ));
        tokio::select! {
            result = &mut log_handle => result??,
            signal = ctrl_c() => {
                signal.context("failed to listen for Ctrl-C")?;
                let _ = log_stop_tx.send(true);
                log_handle.await??;
            }
        }
    } else {
        if backlog.is_empty() && mode == OutputMode::Table {
            if args.processes.is_empty() {
                eprintln!("(no log output yet)");
            } else {
                eprintln!(
                    "(no log output for: {}. Check `decompose ps` for available services.)",
                    args.processes.join(", ")
                );
            }
        }
        if mode == OutputMode::Json {
            for record in backlog {
                record.write(mode, false)?;
            }
        } else {
            let lines: Vec<String> = backlog
                .iter()
                .map(|r| r.render(args.processes.len() == 1))
                .collect();
            let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
            write_logs_maybe_paged(&lines, args.no_pager)?;
        }
    }

    Ok(())
}

/// Write filtered, one-shot log output to stdout, optionally paging through
/// `$PAGER` (or `less -R`) when stdout is a TTY. See [`should_page`] for the
/// gate.
fn write_logs_maybe_paged(lines: &[&str], no_pager: bool) -> Result<()> {
    if should_page(no_pager)
        && let Some(mut child) = spawn_pager()
    {
        let status = {
            let stdin = child.stdin.as_mut();
            if let Some(stdin) = stdin {
                // BrokenPipe just means the user quit the pager — stop
                // writing without treating it as an error.
                let mut bw = std::io::BufWriter::new(stdin);
                for line in lines {
                    if writeln!(bw, "{line}").is_err() {
                        break;
                    }
                }
                let _ = bw.flush();
            }
            // Drop stdin (via the end of this block) so the pager sees
            // EOF and exits. Then wait for it.
            drop(child.stdin.take());
            child.wait()
        };
        let _ = status;
        return Ok(());
    }
    // Falls through to direct stdout on pager spawn failure.
    for line in lines {
        crate::output::write_line(format_args!("{line}"))?;
    }

    Ok(())
}

/// Whether `decompose logs` (one-shot, non-follow) output should be piped
/// through a pager. True iff:
///   - `--no-pager` was not set,
///   - stdout is a TTY,
///   - `$PAGER` is not set to an empty string (matches git's convention for
///     disabling paging via env).
fn should_page(no_pager: bool) -> bool {
    use std::io::IsTerminal;
    if no_pager {
        return false;
    }
    if !std::io::stdout().is_terminal() {
        return false;
    }
    // Explicit empty PAGER / DECOMPOSE_PAGER disables paging (matches git).
    if let Some(v) = env::var_os("DECOMPOSE_PAGER") {
        if v.is_empty() {
            return false;
        }
    } else if let Some(v) = env::var_os("PAGER")
        && v.is_empty()
    {
        return false;
    }
    true
}

/// Spawn the pager subprocess with stdin piped. Honors `DECOMPOSE_PAGER`
/// first, then `PAGER`, falling back to `less -R` (raw control chars so
/// colorized log lines render correctly). Returns `None` on spawn failure so
/// the caller can fall back to direct stdout.
fn spawn_pager() -> Option<std::process::Child> {
    let configured = env::var("DECOMPOSE_PAGER")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| env::var("PAGER").ok().filter(|s| !s.trim().is_empty()));
    let mut cmd = if let Some(command) = configured {
        // Explicit pager commands support shell arguments and pipelines.
        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c").arg(command);
        cmd
    } else {
        let mut cmd = std::process::Command::new("less");
        cmd.arg("-R");
        cmd
    };
    cmd.stdin(std::process::Stdio::piped()).spawn().ok()
}

async fn run_service_command(global: GlobalConfig, args: ServiceArgs, op: ServiceOp) -> Result<()> {
    let paths = resolve_runtime_paths(&global.config_files, global.session.as_deref())?;
    let output_mode = args.output.resolve();

    let daemon = query_daemon(&paths).await?;
    let operation = match op {
        ServiceOp::Start => "start",
        ServiceOp::Stop => "stop",
        ServiceOp::Restart => "restart",
    };
    let request = match op {
        ServiceOp::Start => Request::Start {
            services: args.services.clone(),
        },
        ServiceOp::Stop => Request::Stop {
            services: args.services.clone(),
        },
        ServiceOp::Restart => Request::Restart {
            services: args.services.clone(),
        },
    };

    let stop_budget = if matches!(op, ServiceOp::Stop) {
        Some(shutdown_budget(&paths, None).await?)
    } else {
        None
    };
    let response = match send_request(&paths, request).await {
        Ok(response) => response,
        Err(err) if is_no_daemon_error(&err, &paths) => {
            bail!("no running environment for this project — start one with `decompose up`");
        }
        Err(err) => return Err(err),
    };

    let mut acknowledgment = expect_operation(response)?;
    if let Some(budget) = stop_budget {
        let wait = async {
            loop {
                match send_request(
                    &paths,
                    Request::StopStatus {
                        services: args.services.clone(),
                    },
                )
                .await?
                {
                    Response::StopStatus {
                        complete,
                        errors,
                        failures,
                    } => {
                        if !errors.is_empty() {
                            let failures = if failures.is_empty() {
                                errors
                                    .into_iter()
                                    .map(|e| diagnostic::Diagnostic::error("remote_error", e))
                                    .collect()
                            } else {
                                failures
                            };
                            return Err(diagnostic::Diagnostic::failures(
                                "shutdown_failed",
                                "failed to stop services",
                                failures,
                            )
                            .into());
                        }
                        if complete {
                            return Ok::<(), anyhow::Error>(());
                        }
                    }
                    Response::Error {
                        message,
                        diagnostic,
                    } => return Err(Response::into_error(message, diagnostic)),
                    _ => bail!("unexpected stop status response"),
                }
                sleep(Duration::from_millis(50)).await;
            }
        };
        tokio::time::timeout(budget, wait)
            .await
            .context("timed out waiting for services to stop")??;
    }
    if stop_budget.is_some() {
        acknowledgment.outcome = if acknowledgment
            .services
            .iter()
            .all(|s| s.outcome == ServiceOutcome::AlreadyStopped)
        {
            Outcome::Unchanged
        } else {
            Outcome::Completed
        };
    }
    emit_operation(
        output_mode,
        &OperationResult {
            schema_version: "1.0",
            operation: operation.into(),
            daemon,
            acknowledgment,
            daemon_action: None,
            readiness: None,
            signal: None,
        },
    )?;

    Ok(())
}

async fn run_config(global: GlobalConfig, output_mode: OutputMode) -> Result<()> {
    let cwd = env::current_dir().context("failed to read current directory")?;
    let config_files = resolve_config_paths(&global.config_files, &cwd)?;
    let config_dir = config_files[0].parent().unwrap_or(&cwd).to_path_buf();
    let loaded = load_project(&config_files, &global.env_files, global.disable_dotenv)?;
    let mut cfg = loaded.config;

    let instances = build_process_instances(&cfg, &config_dir, &loaded.dotenv);
    crate::config::validate_resolved_hooks(&instances)?;
    for (name, service) in &mut cfg.processes {
        if let Some(r) = instances.values().find(|r| &r.spec.base_name == name) {
            service.pre_start = Some(r.spec.pre_start.clone());
            service.post_start = Some(r.spec.post_start.clone());
        }
    }
    match output_mode {
        OutputMode::Json => {
            let mut value = serde_json::to_value(&cfg)?;
            value["provenance"] = serde_json::to_value(&loaded.provenance)?;
            let json =
                serde_json::to_string_pretty(&value).context("failed to serialize config")?;
            crate::output::write_line(format_args!("{json}"))?;
        }
        OutputMode::Table => {
            let yaml =
                serde_yaml_ng::to_string(&cfg).context("failed to serialize config as YAML")?;
            crate::output::write_bytes(yaml.as_bytes())?;
        }
    }

    Ok(())
}

async fn run_kill(global: GlobalConfig, args: KillArgs) -> Result<()> {
    let paths = resolve_runtime_paths(&global.config_files, global.session.as_deref())?;
    let output_mode = args.output.resolve();

    let signal = parse_signal(&args.signal)?;
    let daemon = query_daemon(&paths).await?;

    let request = Request::Kill {
        services: args.services.clone(),
        signal,
    };

    let response = match send_request(&paths, request).await {
        Ok(response) => response,
        Err(err) if is_no_daemon_error(&err, &paths) => {
            bail!("no running environment for this project — start one with `decompose up`");
        }
        Err(err) => return Err(err),
    };

    let acknowledgment = expect_operation(response)?;
    emit_operation(
        output_mode,
        &OperationResult {
            schema_version: "1.0",
            operation: "kill".into(),
            daemon,
            acknowledgment,
            daemon_action: None,
            readiness: None,
            signal: Some(output_model::Signal {
                number: signal,
                name: nix::sys::signal::Signal::try_from(signal)
                    .ok()
                    .map(|s| s.as_str().into()),
            }),
        },
    )?;

    Ok(())
}

/// Extract the message from a `Response::Ack`, or bail with an appropriate
/// error for `Response::Error`/unexpected variants. Collapses a match pattern
/// that previously appeared at every "fire-and-acknowledge" IPC callsite.
fn expect_ack(response: Response) -> Result<String> {
    match response {
        Response::Ack { message, .. } => Ok(message),
        Response::Error {
            message,
            diagnostic,
        } => Err(Response::into_error(message, diagnostic)),
        _ => bail!("unexpected response from daemon"),
    }
}

fn parse_signal(s: &str) -> Result<i32> {
    // Accept numeric form (e.g. "9" or "-9").
    if let Ok(num) = s.trim().parse::<i32>() {
        return Ok(num);
    }

    // Accept "SIGTERM" or "TERM" (and "sigterm" / "term"). Nix's
    // `Signal::from_str` only accepts the SIG-prefixed, upper-case form, so
    // normalize into that shape first.
    let upper = s.trim().to_ascii_uppercase();
    let canonical = if upper.starts_with("SIG") {
        upper
    } else {
        format!("SIG{upper}")
    };

    use std::str::FromStr;
    match nix::sys::signal::Signal::from_str(&canonical) {
        Ok(sig) => Ok(sig as i32),
        Err(_) => Err(diagnostic::Diagnostic::error(
            "unknown_signal",
            format!("unknown signal: {s:?} (try e.g. SIGTERM, TERM, 15, or see `kill -l`)"),
        )
        .into()),
    }
}

async fn run_ls(output_mode: OutputMode) -> Result<()> {
    use output_model::{Daemon, DaemonState, DiscoveryResult, Environment};
    let socket_dir = runtime_dir()?;
    let mut environments = Vec::new();
    let entries = match std::fs::read_dir(&socket_dir) {
        Ok(entries) => Some(entries),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error).context("failed to enumerate runtime directory"),
    };
    if let Some(entries) = entries {
        for entry in entries {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("sock") {
                continue;
            }
            let instance = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown")
                .to_string();
            let paths = runtime_paths_for(&instance)?;
            let mut project_dir = None;
            let mut config_files = None;
            let mut process_count = None;
            let (state, pid, diagnostic) = match send_request(&paths, Request::Ping).await {
                Ok(Response::Pong {
                    pid,
                    shutting_down,
                    project_dir: dir,
                    config_files: files,
                    process_count: count,
                    ..
                }) => {
                    project_dir = dir;
                    config_files = files;
                    process_count = count;
                    (
                        if shutting_down {
                            DaemonState::Stopping
                        } else {
                            DaemonState::Running
                        },
                        Some(pid),
                        None,
                    )
                }
                Err(error) if is_no_daemon_error(&error, &paths) => continue,
                Err(error) => (
                    DaemonState::Unreachable,
                    None,
                    Some(diagnostic::Diagnostic::from_error(&error)),
                ),
                Ok(_) => (
                    DaemonState::Unreachable,
                    None,
                    Some(diagnostic::Diagnostic::error(
                        "ipc_unexpected_response",
                        "unexpected response to ping",
                    )),
                ),
            };
            environments.push(Environment {
                daemon: Daemon {
                    state,
                    pid,
                    instance: instance.clone(),
                },
                instance,
                diagnostic,
                project_dir,
                config_files,
                process_count,
            });
        }
    }
    environments.sort_by(|a, b| a.instance.cmp(&b.instance));
    let result = DiscoveryResult {
        schema_version: "1.0",
        environments,
    };
    match output_mode {
        OutputMode::Json => print_json(&result)?,
        OutputMode::Table => {
            if result.environments.is_empty() {
                crate::output::write_line(format_args!("no running environments"))?;
            } else {
                crate::output::write_line(format_args!(
                    "{:<16} {:<14} {:<9} {:<40} config files",
                    "instance", "state", "processes", "project"
                ))?;
                for environment in result.environments {
                    let state = match environment.daemon.state {
                        DaemonState::Running => "running",
                        DaemonState::Stopping => "stopping",
                        _ => "not responding",
                    };
                    let project = environment
                        .project_dir
                        .as_ref()
                        .map(|path| path.display().to_string())
                        .unwrap_or_else(|| "-".into());
                    let files = environment
                        .config_files
                        .as_ref()
                        .map(|files| {
                            files
                                .iter()
                                .map(|path| {
                                    environment
                                        .project_dir
                                        .as_ref()
                                        .and_then(|dir| path.strip_prefix(dir).ok())
                                        .unwrap_or(path)
                                        .display()
                                        .to_string()
                                })
                                .collect::<Vec<_>>()
                                .join(", ")
                        })
                        .unwrap_or_else(|| "-".into());
                    let count = environment
                        .process_count
                        .map(|count| count.to_string())
                        .unwrap_or_else(|| "-".into());
                    crate::output::write_line(format_args!(
                        "{:<16} {state:<14} {count:<9} {project:<40} {files}",
                        environment.instance
                    ))?;
                }
            }
        }
    }
    Ok(())
}

fn resolve_runtime_paths(
    config_files_arg: &[PathBuf],
    session: Option<&str>,
) -> Result<crate::model::RuntimePaths> {
    // Explicit sessions do not depend on local configuration. This also lets
    // IDs from `ls` target daemons from a directory without a compose file.
    let instance = if session.is_some() {
        build_instance_id(session, std::path::Path::new(""), &[])
    } else {
        let cwd = env::current_dir().context("failed to read current directory")?;
        let config_files = resolve_config_paths(config_files_arg, &cwd)?;
        let config_dir = config_files[0].parent().unwrap_or(&cwd);
        build_instance_id(None, config_dir, &config_files)
    };
    runtime_paths_for(&instance)
}

fn filter_log_lines<'a>(lines: &[&'a str], processes: &[String]) -> Vec<&'a str> {
    if processes.is_empty() {
        return lines.to_vec();
    }
    let strip = processes.len() == 1;
    let prefixes: Vec<(String, String)> = processes
        .iter()
        .map(|p| (format!("[{p}] "), format!("[{p}[")))
        .collect();
    lines
        .iter()
        .filter_map(|line| {
            for (plain, replica) in &prefixes {
                if let Some(rest) = line.strip_prefix(plain.as_str()) {
                    return Some(if strip { rest } else { *line });
                }
                if line.starts_with(replica.as_str()) {
                    return Some(if strip {
                        // Replica prefix like `[proc[1]] msg`: strip up to and
                        // including the trailing `] `.
                        line.find("] ").map_or(*line, |end| &line[end + 2..])
                    } else {
                        *line
                    });
                }
            }
            None
        })
        .collect()
}

fn emit_operation(mode: OutputMode, result: &OperationResult) -> Result<()> {
    if mode == OutputMode::Json {
        return print_json(result);
    }
    let message = if result.operation == "down" {
        if result.acknowledgment.outcome == Outcome::Unchanged {
            "daemon not running; nothing to stop".into()
        } else {
            "environment stopped".into()
        }
    } else if matches!(result.readiness, Some(Readiness::Satisfied)) {
        if result.acknowledgment.services.is_empty() {
            "no services to wait for".into()
        } else {
            "all requested services are ready".into()
        }
    } else if result.operation == "up" {
        let changes = result.acknowledgment.changes.as_ref();
        let mut parts = Vec::new();
        if matches!(result.daemon_action, Some(DaemonAction::Started)) {
            parts.push("daemon started".to_string());
        }
        if let Some(changes) = changes {
            let counts = [
                ("added", changes.added.len()),
                ("changed", changes.changed.len()),
                ("removed", changes.removed.len()),
                ("orphans", changes.orphans.len()),
                ("renamed", changes.renamed.len()),
                ("scaled", changes.scaled.len()),
            ];
            let counts = counts
                .into_iter()
                .filter(|(_, count)| *count > 0)
                .map(|(kind, count)| format!("{count} {kind}"))
                .collect::<Vec<_>>();
            if !counts.is_empty() {
                parts.push(format!("configuration reloaded; {}", counts.join(", ")));
            } else if matches!(result.daemon_action, Some(DaemonAction::Reused)) {
                parts.push("no configuration changes".into());
            }
        }
        parts.push(crate::output::operation_summary(&result.acknowledgment));
        parts.join("; ")
    } else {
        let summary = crate::output::operation_summary(&result.acknowledgment);
        if let Some(signal) = &result.signal {
            summary.replace(
                "sent signal to",
                &format!(
                    "sent {} to",
                    signal
                        .name
                        .clone()
                        .unwrap_or_else(|| signal.number.to_string())
                ),
            )
        } else {
            summary
        }
    };
    crate::output::write_line(format_args!("{message}"))?;
    Ok(())
}

async fn query_daemon(paths: &crate::model::RuntimePaths) -> Result<Daemon> {
    match send_request(paths, Request::Ping).await? {
        Response::Pong {
            pid,
            instance,
            shutting_down,
            ..
        } => Ok(Daemon {
            state: if shutting_down {
                DaemonState::Stopping
            } else {
                DaemonState::Running
            },
            pid: Some(pid),
            instance,
        }),
        Response::Error {
            message,
            diagnostic,
        } => Err(Response::into_error(message, diagnostic)),
        _ => Err(diagnostic::Diagnostic::error(
            "ipc_unexpected_response",
            "unexpected response to ping",
        )
        .into()),
    }
}

fn expect_operation(response: Response) -> Result<Acknowledgment> {
    match response {
        Response::Ack {
            result: Some(result),
            ..
        } => Ok(*result),
        Response::Error {
            message,
            diagnostic,
        } => Err(Response::into_error(message, diagnostic)),
        _ => Err(diagnostic::Diagnostic::error(
            "ipc_unexpected_response",
            "daemon did not return a structured operation result",
        )
        .into()),
    }
}

fn emit_session_event(mode: OutputMode, event: output_model::SessionEvent) -> Result<()> {
    #[derive(serde::Serialize)]
    struct Record {
        schema_version: &'static str,
        timestamp: String,
        #[serde(flatten)]
        event: output_model::SessionEvent,
    }
    if mode == OutputMode::Json {
        print_json(&Record {
            schema_version: "1.0",
            timestamp: humantime::format_rfc3339(std::time::SystemTime::now()).to_string(),
            event,
        })
    } else {
        crate::output::write_line(format_args!("{}", event.text()))?;
        Ok(())
    }
}

fn emit_ps(
    mode: OutputMode,
    daemon: output_model::Daemon,
    processes: &[crate::model::ProcessSnapshot],
) -> Result<()> {
    let result = output_model::StatusResult {
        schema_version: "1.0",
        daemon,
        processes,
    };
    match mode {
        OutputMode::Json => {
            print_json(&result)?;
        }
        OutputMode::Table => {
            if let Some(summary) = result.summary() {
                crate::output::write_line(format_args!("{summary}"))?;
            }
            if processes.is_empty() {
                return Ok(());
            }
            let color = use_color();
            let has_replicas = processes.iter().any(|p| p.replica > 1 || p.name != p.base);

            // Build per-row display values.
            let pid_vals: Vec<String> = processes
                .iter()
                .map(|p| {
                    p.pid
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "-".to_string())
                })
                .collect();

            // Build unified state strings for width calculation (glyph + space + label).
            let state_labels: Vec<String> = processes
                .iter()
                .map(|p| {
                    let (g, label, _) =
                        unified_state(&p.state, p.has_readiness_probe, p.ready, false);
                    if label.is_empty() {
                        g.to_string()
                    } else {
                        format!("{g} {label}")
                    }
                })
                .collect();

            // Compute dynamic column widths (minimum = header length).
            let w_name = processes
                .iter()
                .map(|p| p.name.len())
                .max()
                .unwrap_or(0)
                .max("name".len());
            let w_state = state_labels
                .iter()
                .map(|s| s.len())
                .max()
                .unwrap_or(0)
                .max("state".len());
            let w_pid = pid_vals
                .iter()
                .map(|v| v.len())
                .max()
                .unwrap_or(0)
                .max("pid".len());

            if has_replicas {
                let w_base = processes
                    .iter()
                    .map(|p| p.base.len())
                    .max()
                    .unwrap_or(0)
                    .max("base".len());
                crate::output::write_line(format_args!(
                    "{:<w_name$}  {:<w_state$}  {:<w_pid$}  {:<w_base$}",
                    "name", "state", "pid", "base",
                ))?;
                for (i, p) in processes.iter().enumerate() {
                    let (glyph, label, st) =
                        unified_state(&p.state, p.has_readiness_probe, p.ready, color);
                    let cell = if label.is_empty() {
                        glyph.to_string()
                    } else {
                        format!("{glyph} {label}")
                    };
                    crate::output::write_line(format_args!(
                        "{:<w_name$}  {:<w_state$}  {:<w_pid$}  {:<w_base$}  {}",
                        p.name,
                        styled(&cell, st),
                        pid_vals[i],
                        p.base,
                        crate::output::initialization_detail(p),
                    ))?;
                }
            } else {
                crate::output::write_line(format_args!(
                    "{:<w_name$}  {:<w_state$}  {:<w_pid$}",
                    "name", "state", "pid",
                ))?;
                for (i, p) in processes.iter().enumerate() {
                    let (glyph, label, st) =
                        unified_state(&p.state, p.has_readiness_probe, p.ready, color);
                    let cell = if label.is_empty() {
                        glyph.to_string()
                    } else {
                        format!("{glyph} {label}")
                    };
                    crate::output::write_line(format_args!(
                        "{:<w_name$}  {:<w_state$}  {:<w_pid$}  {}",
                        p.name,
                        styled(&cell, st),
                        pid_vals[i],
                        crate::output::initialization_detail(p),
                    ))?;
                }
            }
        }
    }

    Ok(())
}

fn emit_attach(mode: OutputMode) -> Result<()> {
    emit_session_event(
        mode,
        output_model::SessionEvent::Attached {
            ownership: output_model::Ownership::Viewer,
        },
    )
}
fn emit_detach(mode: OutputMode) -> Result<()> {
    emit_session_event(
        mode,
        output_model::SessionEvent::Detached {
            ownership: output_model::Ownership::Viewer,
        },
    )
}

fn cleanup_stale_files(paths: &crate::model::RuntimePaths) {
    let _ = std::fs::remove_file(&paths.socket);
    let _ = std::fs::remove_file(&paths.pid);
    let _ = std::fs::remove_file(&paths.lock);
}

/// Poll until selected services are initialized and started/healthy, bounded by
/// [`tuning::daemon_ready_timeout`] (5 minutes by default).
async fn wait_for_services_ready(
    paths: &crate::model::RuntimePaths,
    selected: &std::collections::HashSet<String>,
    mut progress: Option<&mut WaitProgress>,
) -> Result<()> {
    let mut signals = shutdown::Signals::new()?;
    let deadline = tokio::time::Instant::now() + crate::tuning::daemon_ready_timeout();
    loop {
        let response = send_request(paths, Request::Ps)
            .await
            .context("lost connection to daemon while waiting for services")?;
        let Response::Ps {
            pid,
            instance,
            processes,
            ..
        } = response
        else {
            return Err(diagnostic::Diagnostic::error(
                "ipc_unexpected_response",
                "unexpected response while waiting for services",
            )
            .into());
        };
        if let Some(progress) = progress.as_deref_mut() {
            progress.begin();
            progress.events(paths, selected).await?;
        }
        let eligible = processes.into_iter().filter(|p| {
            selected.contains(&p.base) && p.state != "disabled" && p.state != "not_started"
        });
        let mut pending = Vec::new();
        let mut failures = Vec::new();
        for process in eligible {
            if (process.state != "pending" && process.initialization.failure().is_some())
                || matches!(process.state.as_str(), "failed" | "failed_to_start")
            {
                if let Some(progress) = progress.as_deref_mut() {
                    progress.state(&process, false)?;
                }
                failures.push(process);
            } else {
                let initialized = process.initialization.hooks.is_empty()
                    || process.initialization.state == crate::model::InitializationState::Succeeded;
                let ready = if process.has_readiness_probe {
                    process.ready
                } else {
                    matches!(process.state.as_str(), "running" | "exited")
                };
                if let Some(progress) = progress.as_deref_mut() {
                    progress.state(&process, initialized && ready)?;
                }
                if !initialized && process.state == "exited" {
                    failures.push(process);
                } else if !initialized || !ready {
                    pending.push(process);
                }
            }
        }
        if let Some(progress) = progress.as_deref_mut() {
            progress.render()?;
        }
        let failed = !failures.is_empty();
        if failed || (tokio::time::Instant::now() >= deadline && !pending.is_empty()) {
            let summary = if failed {
                failures
                    .iter()
                    .map(|p| {
                        format!(
                            "{}: {}",
                            p.name,
                            p.initialization
                                .failure()
                                .unwrap_or_else(|| p.status.clone())
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            } else {
                format!(
                    "timed out waiting for service readiness: {}",
                    pending
                        .iter()
                        .map(|p| format!("{}: {}", p.name, crate::output::initialization_detail(p)))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            let mut error = diagnostic::Diagnostic::error(
                if failed {
                    "readiness_failed"
                } else {
                    "readiness_timeout"
                },
                summary,
            );
            error.context = Some(diagnostic::DiagnosticContext {
                operation: Some("up".into()),
                timeout_ms: Some(
                    crate::tuning::daemon_ready_timeout()
                        .as_millis()
                        .min(u64::MAX as u128) as u64,
                ),
                ..Default::default()
            });
            error.details = Some(diagnostic::DiagnosticDetails::Readiness {
                daemon: Daemon {
                    state: DaemonState::Running,
                    pid: Some(pid),
                    instance,
                },
                pending,
                failures,
            });
            return Err(error.into());
        }
        if pending.is_empty() {
            return Ok(());
        }
        let poll = sleep(crate::tuning::daemon_ready_poll());
        tokio::pin!(poll);
        loop {
            let animated = progress.as_deref().is_some_and(WaitProgress::is_inline);
            tokio::select! {
                _ = signals.recv() => return Err(diagnostic::Diagnostic::error("interrupted", "interrupted while waiting for service readiness; environment remains running").into()),
                _ = &mut poll => break,
                _ = sleep(Duration::from_millis(100)), if animated => {
                    if let Some(progress) = progress.as_deref_mut() {
                        progress.render()?;
                    }
                }
            }
        }
    }
}

fn daemon_pid_matches(paths: &crate::model::RuntimePaths, pid: u32) -> bool {
    std::fs::read_to_string(&paths.pid)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        == Some(pid)
        && crate::daemon::parent_alive(pid)
}

fn read_shutdown_receipt(paths: &crate::model::RuntimePaths, pid: u32) -> Result<()> {
    let bytes = std::fs::read(paths.pid.with_extension("shutdown.json"))
        .context("daemon exited without confirming process cleanup")?;
    let receipt: shutdown::Receipt = serde_json::from_slice(&bytes)?;
    if receipt.pid != pid {
        bail!("daemon exited without confirming process cleanup for pid {pid}");
    }
    if !receipt.errors.is_empty() {
        let failures = if receipt.failures.is_empty() {
            receipt
                .errors
                .into_iter()
                .map(|e| diagnostic::Diagnostic::error("remote_error", e))
                .collect()
        } else {
            receipt.failures
        };
        return Err(diagnostic::Diagnostic::failures(
            "shutdown_failed",
            "shutdown cleanup failed",
            failures,
        )
        .into());
    }
    Ok(())
}

async fn wait_for_daemon_stop(
    paths: &crate::model::RuntimePaths,
    pid: u32,
    budget: Duration,
) -> Result<()> {
    tokio::time::timeout(budget, async {
        while daemon_pid_matches(paths, pid) {
            sleep(Duration::from_millis(50)).await;
        }
        read_shutdown_receipt(paths, pid)
    })
    .await
    .context("timed out waiting for daemon cleanup")?
}

fn is_no_daemon_error(err: &anyhow::Error, paths: &crate::model::RuntimePaths) -> bool {
    // Only concrete missing/refused transport errors can establish absence.
    // A timeout or an unreadable PID file must never permit stale cleanup.
    let missing = err
        .chain()
        .filter_map(|cause| cause.downcast_ref::<std::io::Error>())
        .any(|e| {
            matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            )
        });
    if !missing {
        return false;
    }
    match std::fs::read_to_string(&paths.pid) {
        Ok(pid) => pid
            .trim()
            .parse::<u32>()
            .is_ok_and(|pid| !crate::daemon::parent_alive(pid)),
        Err(e) => e.kind() == std::io::ErrorKind::NotFound,
    }
}

async fn stream_daemon_logs(
    log_path: PathBuf,
    mut stop_rx: watch::Receiver<bool>,
    start_at_end: bool,
    mode: OutputMode,
) -> Result<()> {
    let mut reader = crate::logs::Reader::default();
    if start_at_end {
        reader.poll_records(&log_path, &[], Some(0)).await?;
    }
    loop {
        for record in reader.poll_records(&log_path, &[], None).await? {
            record.write(mode, false)?;
        }
        if *stop_rx.borrow() {
            break;
        }
        tokio::select! {
            _ = stop_rx.changed() => {},
            _ = sleep(Duration::from_millis(100)) => {},
        }
    }
    Ok(())
}

async fn stream_filtered_logs(
    paths: crate::model::RuntimePaths,
    mut stop_rx: watch::Receiver<bool>,
    processes: Vec<String>,
    mut reader: crate::logs::Reader,
    mode: OutputMode,
) -> Result<()> {
    let mut poll_counter = 0u32;
    loop {
        for line in reader
            .poll_records(&paths.daemon_log, &processes, None)
            .await?
        {
            line.write(mode, processes.len() == 1)?;
        }
        let _ = std::io::stdout().flush();
        if *stop_rx.borrow() {
            break;
        }
        if !processes.is_empty() {
            poll_counter += 1;
            if poll_counter.is_multiple_of(10)
                && let Ok(Response::Ps {
                    processes: snapshots,
                    ..
                }) = send_request(&paths, Request::Ps).await
            {
                let all_exited = processes.iter().all(|p| {
                    snapshots
                        .iter()
                        .filter(|s| s.base == *p || s.name == *p)
                        .all(|s| s.state == "exited" || s.state == "failed")
                });
                if all_exited {
                    break;
                }
            }
        }
        tokio::select! {
            _ = stop_rx.changed() => {},
            _ = sleep(Duration::from_millis(100)) => {},
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_signal_accepts_numeric_form() {
        assert_eq!(parse_signal("9").unwrap(), 9);
        assert_eq!(parse_signal("15").unwrap(), 15);
        assert_eq!(parse_signal(" 2 ").unwrap(), 2);
    }

    #[test]
    fn parse_signal_accepts_sig_prefixed_name() {
        assert_eq!(parse_signal("SIGTERM").unwrap(), 15);
        assert_eq!(parse_signal("SIGKILL").unwrap(), 9);
        assert_eq!(parse_signal("SIGHUP").unwrap(), 1);
        assert_eq!(parse_signal("SIGINT").unwrap(), 2);
    }

    #[test]
    fn parse_signal_accepts_bare_name() {
        assert_eq!(parse_signal("TERM").unwrap(), 15);
        assert_eq!(parse_signal("KILL").unwrap(), 9);
        assert_eq!(parse_signal("HUP").unwrap(), 1);
        assert_eq!(
            parse_signal("USR1").unwrap(),
            nix::sys::signal::SIGUSR1 as i32
        );
        assert_eq!(
            parse_signal("USR2").unwrap(),
            nix::sys::signal::SIGUSR2 as i32
        );
    }

    #[test]
    fn parse_signal_is_case_insensitive() {
        assert_eq!(parse_signal("sigterm").unwrap(), 15);
        assert_eq!(parse_signal("term").unwrap(), 15);
        assert_eq!(parse_signal("SigKill").unwrap(), 9);
    }

    #[test]
    fn parse_signal_supports_expanded_signal_set() {
        // Sample signals that the old hardcoded implementation did *not*
        // support, to guard against regressing back to the short list.
        assert!(parse_signal("SIGCHLD").is_ok());
        assert!(parse_signal("SIGALRM").is_ok());
        assert!(parse_signal("SIGPIPE").is_ok());
        assert!(parse_signal("SIGTTIN").is_ok());
        assert!(parse_signal("SIGSEGV").is_ok());
    }

    #[test]
    fn parse_signal_unknown_signal_returns_clear_error() {
        let err = parse_signal("NOPESIG").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unknown signal"), "error was: {msg}");
        assert!(msg.contains("NOPESIG"), "error was: {msg}");
    }

    #[test]
    fn parse_signal_empty_string_fails_clearly() {
        let err = parse_signal("").unwrap_err();
        assert!(err.to_string().contains("unknown signal"));
    }
}

#[cfg(test)]
mod boundary_tests {
    use super::*;
    struct FailingWriter(std::io::ErrorKind);
    impl std::io::Write for FailingWriter {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(self.0.into())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    #[tokio::test]
    async fn boundary_writes_to_injected_writers_and_handles_closed_consumers() {
        let args = || {
            ["decompose", "--json", "completion", "bash"]
                .into_iter()
                .map(Into::into)
        };
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        assert_eq!(run_cli_from(args(), &mut stdout, &mut stderr).await, 0);
        assert!(!stdout.is_empty());
        assert!(stderr.is_empty());
        assert_eq!(
            run_cli_from(
                args(),
                &mut FailingWriter(std::io::ErrorKind::BrokenPipe),
                &mut stderr
            )
            .await,
            0
        );
        assert!(stderr.is_empty());
        assert_eq!(
            run_cli_from(
                args(),
                &mut FailingWriter(std::io::ErrorKind::PermissionDenied),
                &mut stderr
            )
            .await,
            1
        );
        let error: serde_json::Value = serde_json::from_slice(&stderr).unwrap();
        assert_eq!(error["severity"], "error");
    }
}
