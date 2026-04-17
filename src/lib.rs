pub mod cli;
pub mod config;
pub mod daemon;
pub mod ipc;
pub mod model;
pub mod output;
pub mod paths;
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

use crate::cli::{Cli, Commands, KillArgs, LogsArgs, ServiceArgs, UpArgs};
use crate::config::{load_and_merge_configs, resolve_config_paths};
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
        Commands::Logs(args) => run_logs(global, args).await,
        Commands::Start(args) => run_service_command(global, args, ServiceOp::Start).await,
        Commands::Stop(args) => run_service_command(global, args, ServiceOp::Stop).await,
        Commands::Restart(args) => run_service_command(global, args, ServiceOp::Restart).await,
        Commands::Config(args) => run_config(global, args.output.resolve()).await,
        Commands::Kill(args) => run_kill(global, args).await,
        Commands::Ls(args) => run_ls(args.output.resolve()).await,
        Commands::Daemon(args) => run_daemon(args).await,
    }
}

#[derive(Debug, Clone, Copy)]
enum ServiceOp {
    Start,
    Stop,
    Restart,
}

async fn run_up(global: GlobalConfig, args: UpArgs) -> Result<()> {
    let output_mode = args.output.resolve();
    let attached = !args.detach;
    let mut got_ctrl_c = false;
    let ctrl_c_task = if attached {
        Some(tokio::spawn(async {
            let _ = ctrl_c().await;
        }))
    } else {
        None
    };
    let cwd = env::current_dir().context("failed to read current directory")?;
    let config_files = resolve_config_paths(&global.config_files, &cwd)?;
    let config_dir = config_files[0].parent().unwrap_or(&cwd).to_path_buf();
    let instance = build_instance_id(global.session.as_deref(), &config_dir, &config_files);
    let paths = runtime_paths_for(&instance)?;
    let mut daemon_pid = None;
    let mut state = "already_running";

    if let Ok(Response::Pong { pid, .. }) = send_request(&paths, Request::Ping).await {
        daemon_pid = Some(pid);
        // Daemon is already running — first trigger a reload so any config
        // edits made since `up` last ran are reconciled (added/changed/removed
        // services). Orphan removal, force-/no-recreate, and no-start are
        // folded into the reload request so the daemon handles them atomically
        // inside the reconcile loop. On parse/validation failure, bail before
        // touching Start.
        let reload_resp = send_request(
            &paths,
            Request::Reload {
                force_recreate: args.force_recreate,
                no_recreate: args.no_recreate,
                remove_orphans: args.remove_orphans,
                no_start: args.no_start,
            },
        )
        .await;
        match reload_resp {
            Ok(Response::Ack { message }) => {
                emit_message(output_mode, "ok", &message);
            }
            Ok(Response::Error { message }) => bail!("{message}"),
            Err(e) => bail!("failed to reload daemon config: {e}"),
            _ => {}
        }

        // Then send a Start request to incrementally bring up the requested
        // services (or all services if none specified). Start is idempotent
        // on already-running processes and picks up any newly-added ones
        // that reload inserted as Pending. Skipped under --no-start: the
        // user explicitly asked us to register-but-not-launch.
        if !args.no_start {
            let start_resp = send_request(
                &paths,
                Request::Start {
                    services: args.processes.clone(),
                },
            )
            .await;
            match start_resp {
                Ok(Response::Ack { .. }) => {}
                Ok(Response::Error { message }) => bail!("{message}"),
                Err(e) => bail!("failed to start services on running daemon: {e}"),
                _ => {}
            }
        }
    } else {
        // Clean up stale socket/pid from a previously killed daemon so the
        // new daemon can bind the socket without interference.
        cleanup_stale_files(&paths);

        // Pre-flight: validate the merged config before spawning the daemon,
        // so users see errors like dependency cycles directly instead of a
        // generic "daemon did not become ready" timeout.
        let preflight = load_and_merge_configs(&config_files)
            .context("config validation failed before starting daemon")?;

        // Validate requested process names exist in config.
        if !args.processes.is_empty() {
            let known: std::collections::HashSet<&str> =
                preflight.processes.keys().map(|k| k.as_str()).collect();
            let unknown: Vec<&str> = args
                .processes
                .iter()
                .filter(|p| !known.contains(p.as_str()))
                .map(|p| p.as_str())
                .collect();
            if !unknown.is_empty() {
                bail!("unknown service(s): {}", unknown.join(", "));
            }
        }

        spawn_daemon_process(
            &cwd,
            &config_files,
            &instance,
            &paths,
            &global.env_files,
            global.disable_dotenv,
            &args.processes,
            args.no_deps,
        )?;
        state = "started";

        for _ in 0..80 {
            if let Ok(Response::Pong { pid, .. }) = send_request(&paths, Request::Ping).await {
                daemon_pid = Some(pid);
                break;
            }
            if let Some(task) = ctrl_c_task.as_ref() {
                if task.is_finished() {
                    got_ctrl_c = true;
                }
            }
            sleep(Duration::from_millis(50)).await;
        }
    }

    let Some(pid) = daemon_pid else {
        bail!(
            "daemon did not become ready; inspect {}",
            paths.daemon_log.display()
        );
    };

    // Orphan removal is folded into the Reload request on the already-running
    // branch. On the freshly-spawned daemon branch there are no orphans yet —
    // the daemon was just initialised from the current config — so a separate
    // RemoveOrphans call here would be a no-op. The standalone
    // Request::RemoveOrphans variant is still used by other code paths.

    emit_up_status(output_mode, state, pid);

    // Print footer block (table mode only).
    if output_mode == OutputMode::Table {
        if let Ok(Response::Ps { processes, .. }) = send_request(&paths, Request::Ps).await {
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
    }

    if !attached {
        if args.wait {
            wait_for_services_ready(&paths, output_mode).await?;
        }
        return Ok(());
    }
    if got_ctrl_c {
        emit_detach(output_mode);
        return Ok(());
    }

    let (log_stop_tx, log_stop_rx) = watch::channel(false);
    let log_handle = tokio::spawn(stream_daemon_logs(
        paths.daemon_log.clone(),
        log_stop_rx,
        state == "already_running",
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
        let (log_stop_tx, log_stop_rx) = watch::channel(false);
        let proc_filter = args.processes.clone();
        let log_handle = tokio::spawn(stream_filtered_logs(
            paths.daemon_log.clone(),
            paths.clone(),
            log_stop_rx,
            proc_filter,
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
        let output = match args.tail {
            Some(n) => {
                let start = filtered.len().saturating_sub(n);
                filtered[start..].to_vec()
            }
            None => filtered,
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
        for line in output {
            println!("{line}");
        }
    }

    Ok(())
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

fn filter_log_lines(lines: &[&str], processes: &[String]) -> Vec<String> {
    if processes.is_empty() {
        return lines.iter().map(|l| l.to_string()).collect();
    }
    let strip = processes.len() == 1;
    lines
        .iter()
        .filter_map(|line| {
            for p in processes {
                let prefix = format!("[{p}] ");
                if line.starts_with(&prefix) {
                    return Some(if strip {
                        line[prefix.len()..].to_string()
                    } else {
                        line.to_string()
                    });
                }
                let prefix2 = format!("[{p}[");
                if line.starts_with(&prefix2) {
                    return Some(if strip {
                        // For replica-style prefix like [proc[1]] msg, strip it
                        if let Some(end) = line.find("] ") {
                            line[end + 2..].to_string()
                        } else {
                            line.to_string()
                        }
                    } else {
                        line.to_string()
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
                        unified_state(&p.state, p.has_readiness_probe, p.healthy, false);
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
                        unified_state(&p.state, p.has_readiness_probe, p.healthy, color);
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
                        unified_state(&p.state, p.has_readiness_probe, p.healthy, color);
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
                            p.healthy
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
) {
    let mut offset = match tokio::fs::metadata(&log_path).await {
        Ok(meta) => meta.len(),
        Err(_) => 0,
    };
    let mut poll_counter: u32 = 0;

    loop {
        if *stop_rx.borrow() {
            break;
        }

        // Periodically check if filtered processes have all exited
        if !processes.is_empty() {
            poll_counter += 1;
            if poll_counter % 10 == 0 {
                if let Ok(Response::Ps {
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
