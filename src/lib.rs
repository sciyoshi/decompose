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
//! - [`output`]   — JSON / table formatting and TTY-aware mode resolution.
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
pub mod health_probes;
pub mod ipc;
pub mod model;
pub mod output;
pub mod paths;
pub mod tui;
pub mod tuning;

use std::env;
use std::io::Write as _;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Parser;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::signal::ctrl_c;
use tokio::sync::watch;
use tokio::time::sleep;

use crate::cli::{Cli, Commands, ExecArgs, KillArgs, LogsArgs, RunArgs, ServiceArgs, UpArgs};
use crate::config::{
    apply_interpolation, build_process_instances, load_and_merge_configs, load_dotenv_files,
    resolve_config_paths,
};
use crate::daemon::{run_daemon, spawn_daemon_process};
use crate::ipc::{Request, Response, send_request};
use crate::output::{
    FooterInfo, OutputMode, print_footer, print_json, style_for_status, styled, unified_state,
    use_color,
};
use crate::paths::{build_instance_id, runtime_dir, runtime_paths_for};

/// Global config flags that live on the top-level `Cli` struct.
#[derive(Debug, Clone)]
pub struct GlobalConfig {
    pub config_files: Vec<PathBuf>,
    pub session: Option<String>,
    pub env_files: Vec<PathBuf>,
    pub disable_dotenv: bool,
}

pub async fn run_cli() -> Result<()> {
    let cli = Cli::parse();

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
        Commands::Logs(args) => run_logs(global, args).await,
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
    let dotenv = load_dotenv_files(&cwd, &global.env_files, global.disable_dotenv)?;
    let mut cfg = load_and_merge_configs(&config_files).context("invalid configuration")?;
    apply_interpolation(&mut cfg);
    crate::config::validate_project_paths(&cfg, &cwd)?;
    if !cfg.processes.contains_key(service) {
        let known: Vec<&str> = cfg.processes.keys().map(|k| k.as_str()).collect();
        bail!(
            "unknown service: {service:?} (known services: {})",
            known.join(", ")
        );
    }
    let instances = build_process_instances(&cfg, &cwd, &dotenv);
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
        Some(("", _)) => bail!("invalid -e entry {raw:?}: empty key"),
        Some((k, v)) => Ok((k.to_string(), v.to_string())),
        None => {
            if raw.is_empty() {
                bail!("invalid -e entry: empty string");
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
        std::process::exit(code);
    }
    Ok(())
}

async fn run_exec(global: GlobalConfig, args: ExecArgs) -> Result<()> {
    // `exec` requires a running service. Preflight against the daemon before
    // doing any local work so users get a clear "service not running" error.
    let (_, _, paths) = runtime_context(&global.config_files, global.session.as_deref()).await?;

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
        Response::Error { message } => bail!("{message}"),
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
        std::process::exit(code);
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
        Some(tokio::spawn(async {
            let _ = ctrl_c().await;
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

    let (pid, state, got_ctrl_c) = ensure_daemon_running(
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

    emit_up_status(output_mode, state, pid);
    maybe_print_footer(output_mode, &paths, &global, attached).await;

    if !attached {
        if args.wait {
            wait_for_services_ready(&paths, output_mode).await?;
        }
        if args.tui {
            return tui::run(paths).await;
        }
        return Ok(());
    }
    if got_ctrl_c {
        emit_detach(output_mode);
        return Ok(());
    }

    stream_logs_until_ctrl_c(&paths, output_mode, state == "already_running", ctrl_c_task).await
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
        cwd,
        config_files,
        instance,
        paths,
    })
}

/// Ensure a daemon is running for this project: either reload/start against
/// an existing daemon, or spawn a fresh one. Returns the daemon PID, the
/// textual state ("started" or "already_running"), and a flag indicating
/// whether the user hit Ctrl-C while we were waiting for the new daemon.
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
) -> Result<(u32, &'static str, bool)> {
    if let Ok(Response::Pong { pid, .. }) = send_request(paths, Request::Ping).await {
        reload_and_start_existing_daemon(args, paths, output_mode).await?;
        Ok((pid, "already_running", false))
    } else {
        // Clean up stale socket/pid from a previously killed daemon so the
        // new daemon can bind the socket without interference.
        cleanup_stale_files(paths);
        preflight_validate_config(config_files, &args.processes)?;
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
            parent_pid,
        )?;
        let (pid, got_ctrl_c) = wait_for_daemon_ready(paths, ctrl_c_task).await?;
        Ok((pid, "started", got_ctrl_c))
    }
}

/// Reload and (optionally) start services against an already-running daemon.
/// On parse/validation failure the reload request errors out before the
/// start call, so users see config errors directly.
async fn reload_and_start_existing_daemon(
    args: &UpArgs,
    paths: &crate::model::RuntimePaths,
    output_mode: OutputMode,
) -> Result<()> {
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
    .map_err(|e| anyhow::anyhow!("failed to reload daemon config: {e}"))?;
    let reload_message = expect_ack(reload_resp)?;
    emit_message(output_mode, "ok", &reload_message);

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
        .map_err(|e| anyhow::anyhow!("failed to start services on running daemon: {e}"))?;
        let _ = expect_ack(start_resp)?;
    }
    Ok(())
}

/// Validate the merged config and the requested service names before
/// spawning the daemon, so users see structured errors (dependency cycles,
/// unknown services) instead of a generic "daemon did not become ready"
/// timeout.
fn preflight_validate_config(config_files: &[PathBuf], processes: &[String]) -> Result<()> {
    let preflight = load_and_merge_configs(config_files)
        .context("config validation failed before starting daemon")?;
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
    bail!(
        "daemon did not become ready; inspect {}",
        paths.daemon_log.display()
    );
}

/// Print the table-mode footer describing service/process counts, session,
/// and socket. Silently no-ops in JSON mode or if the daemon doesn't reply.
async fn maybe_print_footer(
    output_mode: OutputMode,
    paths: &crate::model::RuntimePaths,
    global: &GlobalConfig,
    attached: bool,
) {
    if output_mode != OutputMode::Table {
        return;
    }
    let Ok(Response::Ps { processes, .. }) = send_request(paths, Request::Ps).await else {
        return;
    };
    let service_count = {
        let mut bases: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for p in &processes {
            bases.insert(&p.base);
        }
        bases.len()
    };
    let process_count = processes.len();
    print_footer(&FooterInfo {
        service_count,
        process_count,
        session_name: global.session.as_deref(),
        socket_path: &paths.socket,
        attached,
    });
}

/// Stream the daemon log until the Ctrl-C task fires, then stop the log
/// streamer and emit the "detached" marker. Consumes `ctrl_c_task`.
async fn stream_logs_until_ctrl_c(
    paths: &crate::model::RuntimePaths,
    output_mode: OutputMode,
    start_at_end: bool,
    ctrl_c_task: Option<tokio::task::JoinHandle<()>>,
) -> Result<()> {
    let (log_stop_tx, log_stop_rx) = watch::channel(false);
    let log_handle = tokio::spawn(stream_daemon_logs(
        paths.daemon_log.clone(),
        log_stop_rx,
        start_at_end,
    ));
    emit_attach(output_mode);
    if let Some(task) = ctrl_c_task {
        task.await
            .context("failed waiting for Ctrl-C listener task")?;
    }
    let _ = log_stop_tx.send(true);
    let _ = log_handle.await;
    emit_detach(output_mode);
    Ok(())
}

async fn run_down(
    global: GlobalConfig,
    output_mode: OutputMode,
    timeout: Option<u64>,
) -> Result<()> {
    let (_, _, paths) = runtime_context(&global.config_files, global.session.as_deref()).await?;

    let response = match send_request(
        &paths,
        Request::Down {
            timeout_seconds: timeout,
        },
    )
    .await
    {
        Ok(response) => response,
        Err(err) if is_no_daemon_error(&err, &paths) => {
            emit_message(output_mode, "ok", "no running environment");
            return Ok(());
        }
        Err(err) => return Err(err),
    };

    let message = expect_ack(response)?;
    wait_for_daemon_stop(&paths).await;
    emit_message(output_mode, "ok", &message);

    Ok(())
}

async fn run_ps(global: GlobalConfig, output_mode: OutputMode) -> Result<()> {
    let (_, _, paths) = runtime_context(&global.config_files, global.session.as_deref()).await?;
    let response = match send_request(&paths, Request::Ps).await {
        Ok(response) => response,
        Err(err) if is_no_daemon_error(&err, &paths) => {
            emit_ps_empty(output_mode);
            return Ok(());
        }
        Err(err) => return Err(err),
    };

    match response {
        Response::Ps {
            pid: _,
            instance: _instance,
            mut processes,
        } => {
            processes.sort_by(|a, b| a.name.cmp(&b.name));
            emit_ps(output_mode, &processes);
            Ok(())
        }
        Response::Error { message } => bail!("{message}"),
        _ => bail!("unexpected response from daemon"),
    }
}

async fn run_tui(global: GlobalConfig) -> Result<()> {
    let (_, _, paths) = runtime_context(&global.config_files, global.session.as_deref()).await?;
    match send_request(&paths, Request::Ping).await {
        Ok(Response::Pong { .. }) => {}
        _ => bail!(
            "no running environment for this project — start one with `decompose up -d` first"
        ),
    }
    tui::run(paths).await
}

async fn run_attach(global: GlobalConfig, output_mode: OutputMode) -> Result<()> {
    let (_, _, paths) = runtime_context(&global.config_files, global.session.as_deref()).await?;

    match send_request(&paths, Request::Ping).await {
        Ok(Response::Pong { .. }) => {}
        _ => bail!("no running environment for this project — start one with `decompose up`"),
    };

    emit_attach(output_mode);

    let (log_stop_tx, log_stop_rx) = watch::channel(false);
    let log_handle = tokio::spawn(stream_daemon_logs(
        paths.daemon_log.clone(),
        log_stop_rx,
        false,
    ));

    ctrl_c().await.context("failed to listen for Ctrl-C")?;

    let _ = log_stop_tx.send(true);
    let _ = log_handle.await;
    emit_detach(output_mode);
    Ok(())
}

async fn run_logs(global: GlobalConfig, args: LogsArgs) -> Result<()> {
    let (_, _, paths) = runtime_context(&global.config_files, global.session.as_deref()).await?;

    match send_request(&paths, Request::Ping).await {
        Ok(Response::Pong { .. }) => {}
        _ => bail!("no running environment for this project — start one with `decompose up`"),
    };

    if args.follow {
        // Mirror `docker compose logs -f` / `tail -f`: print the existing
        // backlog first, then stream new output. Read the file once and
        // remember its length so the follower resumes at exactly the byte
        // offset where the backlog ended — no drops, no duplicates.
        //
        // `args.tail` controls how much backlog to show:
        //   * None         — all existing lines
        //   * Some(0)      — explicit opt-out (start streaming from now)
        //   * Some(n)      — last n filtered lines
        let skip_backlog = matches!(args.tail, Some(0));
        let start_offset = match tokio::fs::read(&paths.daemon_log).await {
            Ok(bytes) => {
                let len = bytes.len() as u64;
                if !skip_backlog {
                    let text = String::from_utf8_lossy(&bytes);
                    let lines: Vec<&str> = text.lines().collect();
                    let filtered = filter_log_lines(&lines, &args.processes);
                    let backlog = match args.tail {
                        Some(n) => {
                            let start = filtered.len().saturating_sub(n);
                            &filtered[start..]
                        }
                        None => &filtered[..],
                    };
                    // Print directly to stdout (no pager) so the user sees
                    // backlog immediately and new lines stream in live.
                    let stdout = std::io::stdout();
                    let mut out = stdout.lock();
                    for line in backlog {
                        let _ = writeln!(out, "{line}");
                    }
                    let _ = out.flush();
                }
                len
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => 0,
            Err(err) => {
                return Err(err).with_context(|| {
                    format!(
                        "failed to read daemon log at {}",
                        paths.daemon_log.display()
                    )
                });
            }
        };

        let (log_stop_tx, log_stop_rx) = watch::channel(false);
        let proc_filter = args.processes.clone();
        let log_handle = tokio::spawn(stream_filtered_logs(
            paths.daemon_log.clone(),
            paths.clone(),
            log_stop_rx,
            proc_filter,
            Some(start_offset),
        ));
        ctrl_c().await.context("failed to listen for Ctrl-C")?;
        let _ = log_stop_tx.send(true);
        let _ = log_handle.await;
    } else {
        let content = match tokio::fs::read_to_string(&paths.daemon_log).await {
            Ok(content) => content,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                // No log file yet — treat as empty (daemon just started).
                String::new()
            }
            Err(err) => {
                return Err(err).with_context(|| {
                    format!(
                        "failed to read daemon log at {}",
                        paths.daemon_log.display()
                    )
                });
            }
        };
        let lines: Vec<&str> = content.lines().collect();
        let filtered = filter_log_lines(&lines, &args.processes);
        let output: &[&str] = match args.tail {
            Some(n) => {
                let start = filtered.len().saturating_sub(n);
                &filtered[start..]
            }
            None => &filtered[..],
        };
        if output.is_empty() {
            if args.processes.is_empty() {
                eprintln!("(no log output yet)");
            } else {
                eprintln!(
                    "(no log output for: {}. Check `decompose ps` for available services.)",
                    args.processes.join(", ")
                );
            }
        }
        write_logs_maybe_paged(output, args.no_pager);
    }

    Ok(())
}

/// Write filtered, one-shot log output to stdout, optionally paging through
/// `$PAGER` (or `less -R`) when stdout is a TTY. See [`should_page`] for the
/// gate.
fn write_logs_maybe_paged(lines: &[&str], no_pager: bool) {
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
        return;
    }
    // Falls through to direct stdout on pager spawn failure.
    for line in lines {
        println!("{line}");
    }
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
    let cmd_str = env::var("DECOMPOSE_PAGER")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| env::var("PAGER").ok().filter(|s| !s.trim().is_empty()))
        .unwrap_or_else(|| "less -R".to_string());

    // Run the pager via the shell so users can set things like
    // `PAGER="less -FRX"` or `PAGER="bat --paging=always"`.
    std::process::Command::new("sh")
        .arg("-c")
        .arg(&cmd_str)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .ok()
}

async fn run_service_command(global: GlobalConfig, args: ServiceArgs, op: ServiceOp) -> Result<()> {
    let (_, _, paths) = runtime_context(&global.config_files, global.session.as_deref()).await?;
    let output_mode = args.output.resolve();

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

    let response = match send_request(&paths, request).await {
        Ok(response) => response,
        Err(err) if is_no_daemon_error(&err, &paths) => {
            bail!("no running environment for this project — start one with `decompose up`");
        }
        Err(err) => return Err(err),
    };

    let message = expect_ack(response)?;
    emit_message(output_mode, "ok", &message);

    Ok(())
}

async fn run_config(global: GlobalConfig, output_mode: OutputMode) -> Result<()> {
    let cwd = env::current_dir().context("failed to read current directory")?;
    let config_files = resolve_config_paths(&global.config_files, &cwd)?;
    let cfg = load_and_merge_configs(&config_files).context("invalid configuration")?;
    crate::config::validate_project_paths(&cfg, &cwd)?;

    match output_mode {
        OutputMode::Json => {
            let json = serde_json::to_string_pretty(&cfg).context("failed to serialize config")?;
            println!("{json}");
        }
        OutputMode::Table => {
            let yaml =
                serde_yaml_ng::to_string(&cfg).context("failed to serialize config as YAML")?;
            print!("{yaml}");
        }
    }

    Ok(())
}

async fn run_kill(global: GlobalConfig, args: KillArgs) -> Result<()> {
    let (_, _, paths) = runtime_context(&global.config_files, global.session.as_deref()).await?;
    let output_mode = args.output.resolve();

    let signal = parse_signal(&args.signal)?;

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

    let message = expect_ack(response)?;
    emit_message(output_mode, "ok", &message);

    Ok(())
}

/// Extract the message from a `Response::Ack`, or bail with an appropriate
/// error for `Response::Error`/unexpected variants. Collapses a match pattern
/// that previously appeared at every "fire-and-acknowledge" IPC callsite.
fn expect_ack(response: Response) -> Result<String> {
    match response {
        Response::Ack { message } => Ok(message),
        Response::Error { message } => bail!("{message}"),
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
        Err(_) => bail!("unknown signal: {s:?} (try e.g. SIGTERM, TERM, 15, or see `kill -l`)"),
    }
}

async fn run_ls(output_mode: OutputMode) -> Result<()> {
    let socket_dir = runtime_dir()?;

    let mut environments = Vec::new();

    if socket_dir.is_dir() {
        let entries = std::fs::read_dir(&socket_dir).with_context(|| {
            format!("failed to read runtime directory {}", socket_dir.display())
        })?;

        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("sock") {
                continue;
            }
            let name = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown")
                .to_string();

            let paths = runtime_paths_for(&name)?;
            let status = match send_request(&paths, Request::Ping).await {
                Ok(Response::Pong { .. }) => "running",
                _ => "not responding",
            };

            environments.push((name, status));
        }
    }

    environments.sort_by(|a, b| a.0.cmp(&b.0));
    emit_ls(output_mode, &environments);
    Ok(())
}

fn emit_ls(mode: OutputMode, environments: &[(String, &str)]) {
    match mode {
        OutputMode::Json => {
            let envs: Vec<serde_json::Value> = environments
                .iter()
                .map(|(name, status)| {
                    json!({
                        "name": name,
                        "status": status
                    })
                })
                .collect();
            print_json(&json!({ "environments": envs }));
        }
        OutputMode::Table => {
            if environments.is_empty() {
                println!("No running environments");
            } else {
                println!("NAME                             STATUS");
                for (name, status) in environments {
                    println!("{:<32} {status}", name);
                }
            }
        }
    }
}

async fn runtime_context(
    config_files_arg: &[PathBuf],
    session: Option<&str>,
) -> Result<(
    std::path::PathBuf,
    Vec<std::path::PathBuf>,
    crate::model::RuntimePaths,
)> {
    let cwd = env::current_dir().context("failed to read current directory")?;
    let config_files = resolve_config_paths(config_files_arg, &cwd)?;
    let config_dir = config_files[0].parent().unwrap_or(&cwd).to_path_buf();
    let instance = build_instance_id(session, &config_dir, &config_files);
    let paths = runtime_paths_for(&instance)?;
    Ok((cwd, config_files, paths))
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

fn emit_up_status(mode: OutputMode, status: &str, pid: u32) {
    match mode {
        OutputMode::Table => {
            let color = use_color();
            let green = style_for_status("running", color);
            let (glyph, human) = match status {
                "started" => ("\u{2713}", "decompose started"),
                "already_running" => ("\u{2713}", "decompose already running"),
                _ => ("*", "decompose"),
            };
            println!("{} {human} \u{00b7} pid {pid}", styled(glyph, green),);
        }
        OutputMode::Json => print_json(&json!({
            "status": status,
            "pid": pid
        })),
    }
}

fn emit_message(mode: OutputMode, status: &str, message: &str) {
    match mode {
        OutputMode::Table => println!("{message}"),
        OutputMode::Json => print_json(&json!({
            "status": status,
            "message": message
        })),
    }
}

fn emit_ps(mode: OutputMode, processes: &[crate::model::ProcessSnapshot]) {
    match mode {
        OutputMode::Json => {
            print_json(&json!({
                "processes": processes
            }));
        }
        OutputMode::Table => {
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
                .max("NAME".len());
            let w_state = state_labels
                .iter()
                .map(|s| s.len())
                .max()
                .unwrap_or(0)
                .max("STATE".len());
            let w_pid = pid_vals
                .iter()
                .map(|v| v.len())
                .max()
                .unwrap_or(0)
                .max("PID".len());

            if has_replicas {
                let w_base = processes
                    .iter()
                    .map(|p| p.base.len())
                    .max()
                    .unwrap_or(0)
                    .max("BASE".len());
                println!(
                    "{:<w_name$}  {:<w_state$}  {:<w_pid$}  {:<w_base$}",
                    "NAME", "STATE", "PID", "BASE",
                );
                for (i, p) in processes.iter().enumerate() {
                    let (glyph, label, st) =
                        unified_state(&p.state, p.has_readiness_probe, p.ready, color);
                    let cell = if label.is_empty() {
                        glyph.to_string()
                    } else {
                        format!("{glyph} {label}")
                    };
                    println!(
                        "{:<w_name$}  {:<w_state$}  {:<w_pid$}  {:<w_base$}",
                        p.name,
                        styled(&cell, st),
                        pid_vals[i],
                        p.base,
                    );
                }
            } else {
                println!(
                    "{:<w_name$}  {:<w_state$}  {:<w_pid$}",
                    "NAME", "STATE", "PID",
                );
                for (i, p) in processes.iter().enumerate() {
                    let (glyph, label, st) =
                        unified_state(&p.state, p.has_readiness_probe, p.ready, color);
                    let cell = if label.is_empty() {
                        glyph.to_string()
                    } else {
                        format!("{glyph} {label}")
                    };
                    println!(
                        "{:<w_name$}  {:<w_state$}  {:<w_pid$}",
                        p.name,
                        styled(&cell, st),
                        pid_vals[i],
                    );
                }
            }
        }
    }
}

fn emit_ps_empty(mode: OutputMode) {
    match mode {
        OutputMode::Table => println!("No processes running"),
        OutputMode::Json => print_json(&json!({
            "running": false,
            "processes": []
        })),
    }
}

fn emit_attach(mode: OutputMode) {
    match mode {
        OutputMode::Table => println!("attached (Ctrl-C to detach)"),
        OutputMode::Json => print_json(&json!({
            "status": "attached"
        })),
    }
}

fn emit_detach(mode: OutputMode) {
    match mode {
        OutputMode::Table => println!("detached"),
        OutputMode::Json => print_json(&json!({
            "status": "detached"
        })),
    }
}

/// Remove stale socket and PID files left behind by a killed daemon.
///
/// Called when a Ping to the existing socket failed, meaning the daemon is
/// dead.  Cleaning up here (in addition to the daemon's own startup cleanup)
/// avoids races where the new daemon's `remove_file` is beaten by a concurrent
/// `up` invocation.
fn cleanup_stale_files(paths: &crate::model::RuntimePaths) {
    let _ = std::fs::remove_file(&paths.socket);
    let _ = std::fs::remove_file(&paths.pid);
    let _ = std::fs::remove_file(&paths.lock);
}

/// Poll the daemon until all non-disabled processes are started (or healthy,
/// if a readiness probe is configured). Times out after
/// [`tuning::daemon_ready_timeout`] (5 minutes by default; override with
/// `DECOMPOSE_DAEMON_READY_TIMEOUT_MS`).
async fn wait_for_services_ready(
    paths: &crate::model::RuntimePaths,
    output_mode: OutputMode,
) -> Result<()> {
    let poll_interval = crate::tuning::daemon_ready_poll();
    let timeout = crate::tuning::daemon_ready_timeout();

    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        if tokio::time::Instant::now() >= deadline {
            bail!("timed out waiting for services to become ready");
        }

        match send_request(paths, Request::Ps).await {
            Ok(Response::Ps { processes, .. }) => {
                let active: Vec<&crate::model::ProcessSnapshot> = processes
                    .iter()
                    .filter(|p| p.state != "disabled" && p.state != "not_started")
                    .collect();

                let all_ready = !active.is_empty()
                    && active.iter().all(|p| {
                        if p.state == "failed" {
                            // Already failed — no point waiting.
                            return true;
                        }
                        if p.has_readiness_probe {
                            p.ready
                        } else {
                            p.state == "running" || p.state == "exited"
                        }
                    });

                if all_ready {
                    let any_failed = active.iter().any(|p| p.state == "failed");
                    if any_failed {
                        emit_message(output_mode, "error", "services ready (some failed)");
                    } else {
                        emit_message(output_mode, "ok", "all services are ready");
                    }
                    return Ok(());
                }
            }
            Ok(_) => {}
            Err(_) => {
                // Daemon may have crashed.
                bail!("lost connection to daemon while waiting for services");
            }
        }

        sleep(poll_interval).await;
    }
}

async fn wait_for_daemon_stop(paths: &crate::model::RuntimePaths) {
    for _ in 0..60 {
        if send_request(paths, Request::Ping).await.is_err() {
            break;
        }
        sleep(Duration::from_millis(25)).await;
    }
}

fn is_no_daemon_error(err: &anyhow::Error, paths: &crate::model::RuntimePaths) -> bool {
    if !paths.socket.exists() {
        return true;
    }
    // Walk the full anyhow error chain — the root cause (e.g. "Connection
    // refused") is typically nested inside a context like "failed to connect
    // to /path/to/socket".
    for cause in err.chain() {
        let msg = cause.to_string().to_ascii_lowercase();
        if msg.contains("connection refused")
            || msg.contains("no such file or directory")
            || msg.contains("not found")
            || msg.contains("timed out")
        {
            return true;
        }
    }
    false
}

/// Read new bytes appended to `log_path` since `offset`, returning the updated
/// offset.  Returns `None` when the file hasn't grown (or doesn't exist yet).
async fn read_new_log_bytes(log_path: &std::path::Path, offset: &mut u64) -> Option<Vec<u8>> {
    let meta = tokio::fs::metadata(log_path).await.ok()?;
    let len = meta.len();
    if len < *offset {
        *offset = 0;
    }
    if len <= *offset {
        return None;
    }
    let mut file = tokio::fs::File::open(log_path).await.ok()?;
    file.seek(std::io::SeekFrom::Start(*offset)).await.ok()?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).await.ok()?;
    *offset += buf.len() as u64;
    if buf.is_empty() { None } else { Some(buf) }
}

async fn stream_daemon_logs(
    log_path: std::path::PathBuf,
    mut stop_rx: watch::Receiver<bool>,
    start_at_end: bool,
) {
    let mut offset = match tokio::fs::metadata(&log_path).await {
        Ok(meta) if start_at_end => meta.len(),
        Ok(_) => 0,
        Err(_) => 0,
    };

    loop {
        if *stop_rx.borrow() {
            break;
        }

        if let Some(buf) = read_new_log_bytes(&log_path, &mut offset).await {
            let text = String::from_utf8_lossy(&buf);
            print!("{text}");
            let _ = std::io::stdout().flush();
        }

        tokio::select! {
            _ = stop_rx.changed() => {
                if *stop_rx.borrow() {
                    break;
                }
            }
            _ = sleep(Duration::from_millis(100)) => {}
        }
    }
}

async fn stream_filtered_logs(
    log_path: std::path::PathBuf,
    paths: crate::model::RuntimePaths,
    mut stop_rx: watch::Receiver<bool>,
    processes: Vec<String>,
    // `Some(offset)` starts tailing at the given byte offset (used after a
    // backlog print so we resume exactly where that read ended). `None`
    // preserves the old behaviour of starting at the current end-of-file.
    start_offset: Option<u64>,
) {
    let mut offset = match start_offset {
        Some(off) => off,
        None => match tokio::fs::metadata(&log_path).await {
            Ok(meta) => meta.len(),
            Err(_) => 0,
        },
    };
    let mut poll_counter: u32 = 0;

    loop {
        if *stop_rx.borrow() {
            break;
        }

        // Periodically check if filtered processes have all exited
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

        if let Some(buf) = read_new_log_bytes(&log_path, &mut offset).await {
            let text = String::from_utf8_lossy(&buf);
            let lines: Vec<&str> = text.lines().collect();
            let filtered = filter_log_lines(&lines, &processes);
            for line in filtered {
                println!("{line}");
            }
            let _ = std::io::stdout().flush();
        }

        tokio::select! {
            _ = stop_rx.changed() => {
                if *stop_rx.borrow() {
                    break;
                }
            }
            _ = sleep(Duration::from_millis(100)) => {}
        }
    }
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
