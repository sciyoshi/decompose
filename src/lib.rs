pub mod cli;
pub mod config;
pub mod daemon;
pub mod ipc;
pub mod model;
pub mod output;
pub mod paths;

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

use crate::cli::{
    Cli, Commands, GlobalArgs, LogsArgs, PortsCommand, ProcessCommand, ServiceArgs, UpArgs,
};
use crate::config::resolve_config_paths;
use crate::daemon::{run_daemon, spawn_daemon_process};
use crate::ipc::{PortsRequest, Request, Response, send_request};
use crate::output::{OutputMode, print_json};
use crate::paths::{build_instance_id, runtime_paths_for};

pub async fn run_cli() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Up(args) => run_up(args).await,
        Commands::Down(args) => run_down(args).await,
        Commands::Ps(args) => run_ps(args).await,
        Commands::Attach(args) => run_attach(args).await,
        Commands::Logs(args) => run_logs(args).await,
        Commands::Start(args) => run_service_command(args, ServiceOp::Start).await,
        Commands::Stop(args) => run_service_command(args, ServiceOp::Stop).await,
        Commands::Restart(args) => run_service_command(args, ServiceOp::Restart).await,
        Commands::Process { global, command } => run_process(global, command).await,
        Commands::Ports { global, command } => run_ports(global, command).await,
        Commands::Daemon(args) => run_daemon(args).await,
    }
}

#[derive(Debug, Clone, Copy)]
enum ServiceOp {
    Start,
    Stop,
    Restart,
}

async fn run_up(args: UpArgs) -> Result<()> {
    let output_mode = args.global.output.resolve();
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
    let config_files = resolve_config_paths(&args.global.config_files, &cwd)?;
    let config_dir = config_files[0].parent().unwrap_or(&cwd).to_path_buf();
    let instance = build_instance_id(args.global.session.as_deref(), &config_dir, &config_files);
    let paths = runtime_paths_for(&instance)?;
    let mut daemon_pid = None;
    let mut state = "already_running";

    if let Ok(Response::Pong { pid, .. }) = send_request(&paths, Request::Ping).await {
        daemon_pid = Some(pid);
    } else {
        spawn_daemon_process(
            &cwd,
            &config_files,
            &instance,
            &paths,
            &args.global.env_files,
            args.global.disable_dotenv,
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

    emit_up_status(output_mode, state, pid);
    if !attached {
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

async fn run_down(args: GlobalArgs) -> Result<()> {
    let (_, _, paths) = runtime_context(&args.config_files, args.session.as_deref()).await?;
    let output_mode = args.output.resolve();

    let response = match send_request(&paths, Request::Down).await {
        Ok(response) => response,
        Err(err) if is_no_daemon_error(&err, &paths) => {
            emit_message(output_mode, "ok", "no running environment");
            return Ok(());
        }
        Err(err) => return Err(err),
    };

    match response {
        Response::Ack { message } => {
            wait_for_daemon_stop(&paths).await;
            emit_message(output_mode, "ok", &message);
        }
        Response::Error { message } => bail!("{message}"),
        _ => bail!("unexpected response from daemon"),
    }

    Ok(())
}

async fn run_ps(args: GlobalArgs) -> Result<()> {
    let (_, _, paths) = runtime_context(&args.config_files, args.session.as_deref()).await?;
    let output_mode = args.output.resolve();
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

async fn run_attach(args: GlobalArgs) -> Result<()> {
    let (_, _, paths) = runtime_context(&args.config_files, args.session.as_deref()).await?;
    let output_mode = args.output.resolve();

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

async fn run_logs(args: LogsArgs) -> Result<()> {
    let (_, _, paths) = runtime_context(
        &args.global.config_files,
        args.global.session.as_deref(),
    )
    .await?;

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
        let content = tokio::fs::read_to_string(&paths.daemon_log)
            .await
            .unwrap_or_default();
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

async fn run_process(args: GlobalArgs, command: ProcessCommand) -> Result<()> {
    let (_, _, paths) = runtime_context(&args.config_files, args.session.as_deref()).await?;
    let output_mode = args.output.resolve();

    let request = match command {
        ProcessCommand::Scale { process, replicas } => Request::Scale { process, replicas },
    };

    let response = send_request(&paths, request).await?;

    match response {
        Response::Ack { message } => emit_message(output_mode, "ok", &message),
        Response::Error { message } => bail!("{message}"),
        _ => bail!("unexpected response from daemon"),
    }

    Ok(())
}

async fn run_service_command(args: ServiceArgs, op: ServiceOp) -> Result<()> {
    let (_, _, paths) =
        runtime_context(&args.global.config_files, args.global.session.as_deref()).await?;
    let output_mode = args.global.output.resolve();

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

    let response = send_request(&paths, request).await?;

    match response {
        Response::Ack { message } => emit_message(output_mode, "ok", &message),
        Response::Error { message } => bail!("{message}"),
        _ => bail!("unexpected response from daemon"),
    }

    Ok(())
}

async fn run_ports(args: GlobalArgs, command: PortsCommand) -> Result<()> {
    let (_, _, paths) = runtime_context(&args.config_files, args.session.as_deref()).await?;
    let output_mode = args.output.resolve();
    let response = send_request(
        &paths,
        Request::Ports {
            command: match command {
                PortsCommand::List => PortsRequest::List,
                PortsCommand::Free => PortsRequest::Free,
                PortsCommand::Release { service_name } => PortsRequest::Release { service_name },
                PortsCommand::Reserve { port, service_name } => {
                    PortsRequest::Reserve { port, service_name }
                }
                PortsCommand::Inspect { service_name } => PortsRequest::Inspect { service_name },
            },
        },
    )
    .await?;

    match response {
        Response::Ack { message } => emit_message(output_mode, "ok", &message),
        Response::Error { message } => bail!("{message}"),
        _ => bail!("unexpected response from daemon"),
    }

    Ok(())
}

async fn runtime_context(
    config_files_arg: &[PathBuf],
    session: Option<&str>,
) -> Result<(std::path::PathBuf, Vec<std::path::PathBuf>, crate::model::RuntimePaths)> {
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
            let human = match status {
                "started" => "started",
                "already_running" => "already running",
                other => other,
            };
            println!("decompose {human} (pid {pid})");
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
            let has_replicas = processes.iter().any(|p| p.replica > 1 || p.name != p.base);
            if has_replicas {
                println!("NAME                     STATUS               BASE");
                for p in processes {
                    println!("{:<24} {:<20} {}", p.name, p.status, p.base);
                }
            } else {
                println!("NAME                     STATUS");
                for p in processes {
                    println!("{:<24} {}", p.name, p.status);
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
    let msg = err.to_string().to_ascii_lowercase();
    msg.contains("connection refused")
        || msg.contains("no such file or directory")
        || msg.contains("not found")
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

        if let Ok(meta) = tokio::fs::metadata(&log_path).await {
            let len = meta.len();
            if len < offset {
                offset = 0;
            }
            if len > offset {
                if let Ok(mut file) = tokio::fs::File::open(&log_path).await {
                    if file.seek(std::io::SeekFrom::Start(offset)).await.is_ok() {
                        let mut buf = Vec::new();
                        if file.read_to_end(&mut buf).await.is_ok() {
                            offset += buf.len() as u64;
                            if !buf.is_empty() {
                                let text = String::from_utf8_lossy(&buf);
                                print!("{text}");
                                let _ = std::io::stdout().flush();
                            }
                        }
                    }
                }
            }
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
                            .all(|s| {
                                s.state == "exited" || s.state == "failed"
                            })
                    });
                    if all_exited {
                        break;
                    }
                }
            }
        }

        if let Ok(meta) = tokio::fs::metadata(&log_path).await {
            let len = meta.len();
            if len < offset {
                offset = 0;
            }
            if len > offset {
                if let Ok(mut file) = tokio::fs::File::open(&log_path).await {
                    if file.seek(std::io::SeekFrom::Start(offset)).await.is_ok() {
                        let mut buf = Vec::new();
                        if file.read_to_end(&mut buf).await.is_ok() {
                            offset += buf.len() as u64;
                            if !buf.is_empty() {
                                let text = String::from_utf8_lossy(&buf);
                                let lines: Vec<&str> = text.lines().collect();
                                let filtered = filter_log_lines(&lines, &processes);
                                for line in filtered {
                                    println!("{line}");
                                }
                                let _ = std::io::stdout().flush();
                            }
                        }
                    }
                }
            }
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
