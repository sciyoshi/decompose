use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::fs::OpenOptions;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use interprocess::local_socket::ListenerOptions;
use interprocess::local_socket::tokio::Stream;
use interprocess::local_socket::traits::tokio::Listener as _;
use regex::Regex;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command as TokioCommand;
use tokio::sync::{Mutex, watch};
use tokio::time::sleep;

use crate::cli::DaemonArgs;
use crate::config::{
    apply_interpolation, build_process_instances, collect_process_subset, load_and_merge_configs,
    load_dotenv_files,
};
use crate::ipc::{Request, Response, to_socket_name};
use crate::model::{
    DependencyCondition, ExitMode, HealthProbe, ProcessRuntime, ProcessSnapshot, ProcessStatus,
    RestartPolicy, RuntimePaths,
};
use crate::paths::runtime_paths_for;

#[derive(Debug)]
struct DaemonState {
    instance: String,
    processes: BTreeMap<String, ProcessRuntime>,
    controllers: BTreeMap<String, watch::Sender<bool>>,
    shutdown_requested: bool,
    exit_mode: ExitMode,
    /// CLI-level override for shutdown timeout (from `down --timeout`).
    shutdown_timeout_override: Option<u64>,
}

type SharedState = Arc<Mutex<DaemonState>>;

/// Acquire an exclusive, non-blocking advisory lock for this daemon instance.
///
/// Returns the open lock file handle — the caller must keep it alive for the
/// daemon's entire lifetime. The lock is automatically released when the
/// file descriptor is closed (including on crash).
#[cfg(unix)]
fn acquire_instance_lock(paths: &RuntimePaths) -> Result<fs::File> {
    let lock_file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&paths.lock)
        .with_context(|| format!("failed to open lock file at {}", paths.lock.display()))?;

    let ret = unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if ret != 0 {
        anyhow::bail!(
            "another daemon is already running for this project (lock held on {})",
            paths.lock.display()
        );
    }
    Ok(lock_file)
}

#[cfg(not(unix))]
fn acquire_instance_lock(paths: &RuntimePaths) -> Result<fs::File> {
    // Advisory locking is Unix-only; on other platforms we skip it.
    // The socket bind below still provides some protection against duplicates.
    OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&paths.lock)
        .with_context(|| format!("failed to open lock file at {}", paths.lock.display()))
}

#[allow(clippy::too_many_arguments)]
pub fn spawn_daemon_process(
    cwd: &Path,
    config_files: &[std::path::PathBuf],
    instance: &str,
    paths: &RuntimePaths,
    env_files: &[std::path::PathBuf],
    disable_dotenv: bool,
    processes: &[String],
    no_deps: bool,
) -> Result<()> {
    let exe = env::current_exe().context("failed to locate current executable")?;
    if let Some(parent) = paths.daemon_log.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&paths.daemon_log)
        .with_context(|| {
            format!(
                "failed to open daemon log at {}",
                paths.daemon_log.display()
            )
        })?;
    writeln!(
        log_file,
        "\n--- daemon started at {} ---",
        humantime::format_rfc3339_seconds(std::time::SystemTime::now())
    )
    .ok();
    let log_err = log_file.try_clone()?;

    let mut cmd = std::process::Command::new(exe);
    cmd.arg("daemon").arg("--cwd").arg(cwd);

    for cf in config_files {
        cmd.arg("--config-file").arg(cf);
    }

    cmd.arg("--instance").arg(instance);

    for ef in env_files {
        cmd.arg("--env-file").arg(ef);
    }

    if disable_dotenv {
        cmd.arg("--disable-dotenv");
    }

    for proc_name in processes {
        cmd.arg("--process").arg(proc_name);
    }

    if no_deps {
        cmd.arg("--no-deps");
    }

    cmd.stdin(Stdio::null())
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(log_err));

    let _child = cmd.spawn().context("failed to spawn daemon process")?;
    Ok(())
}

pub async fn run_daemon(args: DaemonArgs) -> Result<()> {
    env::set_current_dir(&args.cwd).with_context(|| {
        format!(
            "failed to change cwd to {}",
            args.cwd.as_os_str().to_string_lossy()
        )
    })?;

    let paths = runtime_paths_for(&args.instance)?;
    if let Some(parent) = paths.socket.parent() {
        fs::create_dir_all(parent)?;
    }
    if let Some(parent) = paths.pid.parent() {
        fs::create_dir_all(parent)?;
    }

    // Acquire an exclusive advisory lock to prevent duplicate daemons.
    // The lock file descriptor must be held for the lifetime of the daemon;
    // it is automatically released when the process exits (even on crash).
    let _lock_file = acquire_instance_lock(&paths)?;

    // Now that we hold the lock, it's safe to clean up a stale socket from
    // a previously crashed daemon.
    if paths.socket.exists() {
        let _ = fs::remove_file(&paths.socket);
    }

    let dotenv = load_dotenv_files(&args.cwd, &args.env_files, args.disable_dotenv)?;

    let mut config = load_and_merge_configs(&args.config_files)?;
    apply_interpolation(&mut config);

    // Determine which services were selected for launch. Non-selected ones
    // stay in the daemon state as NotStarted so they can be addressed later
    // by `start` or incremental `up`.
    let selected: Option<std::collections::HashSet<String>> = if !args.processes.is_empty() {
        Some(collect_process_subset(
            &config,
            &args.processes,
            !args.no_deps,
        )?)
    } else {
        None
    };

    let exit_mode = config.exit_mode;
    let mut process_map = build_process_instances(&config, &args.cwd, &dotenv);

    // Mark non-selected services as NotStarted instead of Pending so the
    // supervisor won't auto-launch them.
    if let Some(ref selected) = selected {
        for (name, runtime) in process_map.iter_mut() {
            if !selected.contains(&runtime.spec.base_name)
                && !selected.contains(name)
                && matches!(runtime.status, ProcessStatus::Pending)
            {
                runtime.status = ProcessStatus::NotStarted;
            }
        }
    }

    fs::write(&paths.pid, std::process::id().to_string()).with_context(|| {
        format!(
            "failed to write pid file to {}",
            paths.pid.as_path().display()
        )
    })?;

    let socket_name = to_socket_name(&paths.socket)?;
    let listener = ListenerOptions::new()
        .name(socket_name)
        .create_tokio()
        .context("failed to create local socket listener")?;

    let state = Arc::new(Mutex::new(DaemonState {
        instance: args.instance.clone(),
        processes: process_map,
        controllers: BTreeMap::new(),
        shutdown_requested: false,
        exit_mode,
        shutdown_timeout_override: None,
    }));

    let (stop_tx, mut stop_rx) = watch::channel(false);
    tokio::spawn(supervisor_loop(state.clone(), stop_tx));

    loop {
        tokio::select! {
            changed = stop_rx.changed() => {
                if changed.is_ok() && *stop_rx.borrow() {
                    break;
                }
            }
            incoming = listener.accept() => {
                match incoming {
                    Ok(stream) => {
                        let state = state.clone();
                        tokio::spawn(async move {
                            let _ = handle_client(stream, state).await;
                        });
                    }
                    Err(e) => {
                        eprintln!("socket accept error: {e}");
                        sleep(Duration::from_millis(50)).await;
                    }
                }
            }
        }
    }

    let _ = fs::remove_file(&paths.socket);
    let _ = fs::remove_file(&paths.pid);
    let _ = fs::remove_file(&paths.lock);
    Ok(())
}

async fn supervisor_loop(state: SharedState, stop_tx: watch::Sender<bool>) {
    loop {
        let mut launchable = Vec::new();
        let mut request_shutdown = false;

        {
            let mut guard = state.lock().await;
            let snapshot = guard.processes.clone();

            // Check exit mode triggers
            if !guard.shutdown_requested {
                let triggered = match guard.exit_mode {
                    ExitMode::WaitAll => false,
                    ExitMode::ExitOnFailure => snapshot.values().any(|p| {
                        matches!(p.status, ProcessStatus::Exited { code } if code != 0)
                            || matches!(p.status, ProcessStatus::FailedToStart { .. })
                    }),
                    ExitMode::ExitOnEnd => snapshot
                        .values()
                        .any(|p| matches!(p.status, ProcessStatus::Exited { .. })),
                };
                if triggered {
                    drop(guard);
                    let mut guard = state.lock().await;
                    guard.shutdown_requested = true;
                    for tx in guard.controllers.values() {
                        let _ = tx.send(true);
                    }
                    // Pending processes have no controller — stop them directly
                    for runtime in guard.processes.values_mut() {
                        if matches!(runtime.status, ProcessStatus::Pending) {
                            runtime.status = ProcessStatus::Stopped;
                        }
                    }
                    request_shutdown = true;
                } else {
                    for (name, proc_runtime) in &snapshot {
                        if !matches!(proc_runtime.status, ProcessStatus::Pending) {
                            continue;
                        }
                        if proc_runtime.spec.disabled {
                            continue;
                        }
                        if dependencies_met(proc_runtime, &snapshot) {
                            launchable.push(name.clone());
                        }
                    }

                    if guard.shutdown_requested {
                        request_shutdown = true;
                        for tx in guard.controllers.values() {
                            let _ = tx.send(true);
                        }
                        // Pending processes have no controller — stop them directly
                        for runtime in guard.processes.values_mut() {
                            if matches!(runtime.status, ProcessStatus::Pending) {
                                runtime.status = ProcessStatus::Stopped;
                            }
                        }
                    }
                }
            } else {
                request_shutdown = true;
                for tx in guard.controllers.values() {
                    let _ = tx.send(true);
                }
                // Pending processes have no controller — stop them directly
                for runtime in guard.processes.values_mut() {
                    if matches!(runtime.status, ProcessStatus::Pending) {
                        runtime.status = ProcessStatus::Stopped;
                    }
                }
            }
        }

        for name in launchable {
            start_process(name, state.clone()).await;
        }

        let done = {
            let guard = state.lock().await;
            request_shutdown
                && guard
                    .processes
                    .values()
                    .all(|runtime| runtime.status.is_terminal())
        };

        if done {
            let _ = stop_tx.send(true);
            break;
        }

        sleep(Duration::from_millis(150)).await;
    }
}

pub(crate) fn dependencies_met(
    candidate: &ProcessRuntime,
    snapshot: &BTreeMap<String, ProcessRuntime>,
) -> bool {
    for (dep_base, cond) in &candidate.spec.depends_on {
        let dep_instances: Vec<&ProcessRuntime> = snapshot
            .values()
            .filter(|p| p.spec.base_name == *dep_base)
            .collect();
        if dep_instances.is_empty() {
            return false;
        }

        let satisfied = match cond {
            DependencyCondition::ProcessStarted => dep_instances.iter().all(|p| p.started_once),
            DependencyCondition::ProcessHealthy => dep_instances.iter().all(|p| p.healthy),
            DependencyCondition::ProcessLogReady => dep_instances.iter().all(|p| p.log_ready),
            DependencyCondition::ProcessCompleted => dep_instances.iter().all(|p| {
                matches!(
                    p.status,
                    ProcessStatus::Exited { .. } | ProcessStatus::Stopped
                )
            }),
            DependencyCondition::ProcessCompletedSuccessfully => dep_instances
                .iter()
                .all(|p| matches!(p.status, ProcessStatus::Exited { code: 0 })),
        };

        if !satisfied {
            return false;
        }
    }

    true
}

async fn start_process(name: String, state: SharedState) {
    let spec = {
        let mut guard = state.lock().await;
        let Some(runtime) = guard.processes.get_mut(&name) else {
            return;
        };
        if !matches!(runtime.status, ProcessStatus::Pending) {
            return;
        }
        runtime.spec.clone()
    };

    // Compile ready_log_line pattern
    let ready_pattern: Option<Regex> = spec.ready_log_line.as_ref().map(|pattern| {
        Regex::new(pattern).unwrap_or_else(|_| Regex::new(&regex::escape(pattern)).unwrap())
    });

    let mut cmd = match build_shell_command(&spec.command) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[{name}] failed to build shell command: {e}");
            let mut guard = state.lock().await;
            if let Some(runtime) = guard.processes.get_mut(&name) {
                runtime.status = ProcessStatus::FailedToStart {
                    reason: e.to_string(),
                };
            }
            return;
        }
    };
    cmd.current_dir(&spec.working_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .envs(&spec.environment);

    let spawn_res = cmd.spawn();
    let mut child = match spawn_res {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[{name}] failed to start: {e}");
            let mut guard = state.lock().await;
            if let Some(runtime) = guard.processes.get_mut(&name) {
                runtime.status = ProcessStatus::FailedToStart {
                    reason: e.to_string(),
                };
            }
            return;
        }
    };

    let pid = child.id().unwrap_or(0);
    {
        let mut guard = state.lock().await;
        if let Some(runtime) = guard.processes.get_mut(&name) {
            runtime.status = ProcessStatus::Running { pid };
            runtime.started_once = true;
        }
    }

    // Spawn health check probes
    if let Some(ref probe) = spec.readiness_probe {
        tokio::spawn(run_readiness_probe(
            name.clone(),
            probe.clone(),
            state.clone(),
            spec.working_dir.clone(),
            spec.environment.clone(),
        ));
    }
    if let Some(ref probe) = spec.liveness_probe {
        tokio::spawn(run_liveness_probe(
            name.clone(),
            probe.clone(),
            state.clone(),
            spec.working_dir.clone(),
            spec.environment.clone(),
        ));
    }

    attach_output_readers(&mut child, &name, ready_pattern, state.clone());

    let (kill_tx, mut kill_rx) = watch::channel(false);
    {
        let mut guard = state.lock().await;
        guard.controllers.insert(name.clone(), kill_tx);
    }

    // Process lifecycle task: handles exit, restart, and shutdown
    tokio::spawn({
        let state = state.clone();
        let name = name.clone();
        async move {
            loop {
                let final_status = tokio::select! {
                    _ = kill_rx.changed() => {
                        if *kill_rx.borrow() {
                            let timeout_override = {
                                let guard = state.lock().await;
                                guard.shutdown_timeout_override
                            };
                            shutdown_child(&mut child, &spec, timeout_override).await;
                            ProcessStatus::Stopped
                        } else {
                            ProcessStatus::Stopped
                        }
                    }
                    wait_res = child.wait() => {
                        match wait_res {
                            Ok(exit_status) => ProcessStatus::Exited {
                                code: exit_status.code().unwrap_or(-1),
                            },
                            Err(e) => ProcessStatus::FailedToStart {
                                reason: format!("wait failed: {e}"),
                            },
                        }
                    }
                };

                // Check if we should restart
                let should_restart = {
                    let mut guard = state.lock().await;
                    if let Some(runtime) = guard.processes.get_mut(&name) {
                        let do_restart = match (&final_status, runtime.spec.restart_policy) {
                            (ProcessStatus::Stopped, _) => false,
                            (_, RestartPolicy::No) => false,
                            (ProcessStatus::Exited { code: 0 }, RestartPolicy::OnFailure) => false,
                            (_, RestartPolicy::OnFailure) | (_, RestartPolicy::Always) => {
                                match runtime.spec.max_restarts {
                                    Some(max) => runtime.restart_count < max,
                                    None => true,
                                }
                            }
                        };
                        if do_restart {
                            runtime.status = ProcessStatus::Restarting;
                            runtime.restart_count += 1;
                            true
                        } else {
                            runtime.status = final_status;
                            false
                        }
                    } else {
                        false
                    }
                };

                if !should_restart {
                    let mut guard = state.lock().await;
                    guard.controllers.remove(&name);
                    break;
                }

                // Backoff delay
                let backoff = {
                    let guard = state.lock().await;
                    guard
                        .processes
                        .get(&name)
                        .map(|r| r.spec.backoff_seconds)
                        .unwrap_or(1)
                };
                sleep(Duration::from_secs(backoff)).await;

                // Re-spawn
                let spec = {
                    let guard = state.lock().await;
                    guard.processes.get(&name).map(|r| r.spec.clone())
                };
                let Some(spec) = spec else { break };

                let mut cmd = match build_shell_command(&spec.command) {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("[{name}] failed to build shell command on restart: {e}");
                        let mut guard = state.lock().await;
                        if let Some(runtime) = guard.processes.get_mut(&name) {
                            runtime.status = ProcessStatus::FailedToStart {
                                reason: e.to_string(),
                            };
                        }
                        guard.controllers.remove(&name);
                        break;
                    }
                };
                cmd.current_dir(&spec.working_dir)
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .envs(&spec.environment);

                match cmd.spawn() {
                    Ok(new_child) => {
                        child = new_child;
                        let pid = child.id().unwrap_or(0);
                        {
                            let mut guard = state.lock().await;
                            if let Some(runtime) = guard.processes.get_mut(&name) {
                                runtime.status = ProcessStatus::Running { pid };
                                runtime.log_ready = false;
                                runtime.healthy = false;
                            }
                        }

                        // Re-attach stdout/stderr readers
                        let ready_pattern: Option<Regex> =
                            spec.ready_log_line.as_ref().map(|pattern| {
                                Regex::new(pattern).unwrap_or_else(|_| {
                                    Regex::new(&regex::escape(pattern)).unwrap()
                                })
                            });
                        attach_output_readers(&mut child, &name, ready_pattern, state.clone());

                        // Create new kill channel for this restart iteration
                        let (new_kill_tx, new_kill_rx) = watch::channel(false);
                        {
                            let mut guard = state.lock().await;
                            guard.controllers.insert(name.clone(), new_kill_tx);
                        }
                        kill_rx = new_kill_rx;
                    }
                    Err(e) => {
                        let mut guard = state.lock().await;
                        if let Some(runtime) = guard.processes.get_mut(&name) {
                            runtime.status = ProcessStatus::FailedToStart {
                                reason: e.to_string(),
                            };
                        }
                        guard.controllers.remove(&name);
                        break;
                    }
                }
            }
        }
    });
}

/// Spawn tasks that read lines from the child's stdout and stderr pipes,
/// printing them with a `[name]` prefix and optionally matching a
/// `ready_log_line` regex to set the `log_ready` flag on the process runtime.
fn attach_output_readers(
    child: &mut tokio::process::Child,
    name: &str,
    ready_pattern: Option<Regex>,
    state: SharedState,
) {
    let log_ready_flag = Arc::new(AtomicBool::new(false));

    if let Some(stdout) = child.stdout.take() {
        let proc_name = name.to_string();
        let pattern = ready_pattern.clone();
        let flag = log_ready_flag.clone();
        let state_clone = state.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                println!("[{proc_name}] {line}");
                if let Some(ref re) = pattern {
                    if !flag.load(Ordering::Relaxed) && re.is_match(&line) {
                        flag.store(true, Ordering::Relaxed);
                        let mut guard = state_clone.lock().await;
                        if let Some(runtime) = guard.processes.get_mut(&proc_name) {
                            runtime.log_ready = true;
                        }
                    }
                }
            }
        });
    }

    if let Some(stderr) = child.stderr.take() {
        let proc_name = name.to_string();
        let pattern = ready_pattern;
        let flag = log_ready_flag;
        let state_clone = state;
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                eprintln!("[{proc_name}] {line}");
                if let Some(ref re) = pattern {
                    if !flag.load(Ordering::Relaxed) && re.is_match(&line) {
                        flag.store(true, Ordering::Relaxed);
                        let mut guard = state_clone.lock().await;
                        if let Some(runtime) = guard.processes.get_mut(&proc_name) {
                            runtime.log_ready = true;
                        }
                    }
                }
            }
        });
    }
}

async fn shutdown_child(
    child: &mut tokio::process::Child,
    spec: &crate::model::ProcessInstanceSpec,
    timeout_override: Option<u64>,
) {
    // Step 1: Run optional shutdown command
    if let Some(ref cmd_str) = spec.shutdown_command {
        match build_shell_command(cmd_str) {
            Ok(mut cmd) => {
                cmd.current_dir(&spec.working_dir).envs(&spec.environment);
                let _ = cmd.output().await;
            }
            Err(e) => {
                eprintln!("[{}] shutdown command failed to build: {e}", spec.name);
            }
        }
    }

    // Step 2: Send signal
    let signal = spec.shutdown_signal.unwrap_or(15);
    if let Some(pid) = child.id() {
        #[cfg(unix)]
        {
            use nix::sys::signal::{self, Signal};
            use nix::unistd::Pid;
            if let Ok(sig) = Signal::try_from(signal) {
                let _ = signal::kill(Pid::from_raw(pid as i32), sig);
            }
        }
        #[cfg(not(unix))]
        {
            let _ = signal; // suppress unused warning
            let _ = child.start_kill();
        }
    }

    // Step 3: Wait with timeout
    let timeout = Duration::from_secs(timeout_override.unwrap_or(spec.shutdown_timeout_seconds));
    match tokio::time::timeout(timeout, child.wait()).await {
        Ok(_) => {}
        Err(_) => {
            // Step 4: Force kill
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
    }
}

/// Run a readiness probe periodically. Sets the `healthy` flag on the process
/// runtime when the probe succeeds (and clears it when the failure threshold
/// is reached).
async fn run_readiness_probe(
    name: String,
    probe: HealthProbe,
    state: SharedState,
    working_dir: std::path::PathBuf,
    environment: BTreeMap<String, String>,
) {
    // Initial delay
    if probe.initial_delay_seconds > 0 {
        sleep(Duration::from_secs(probe.initial_delay_seconds)).await;
    }

    let mut consecutive_successes: u32 = 0;
    let mut consecutive_failures: u32 = 0;

    loop {
        // Check if process is still running
        {
            let guard = state.lock().await;
            if let Some(runtime) = guard.processes.get(&name) {
                if runtime.status.is_terminal() {
                    break;
                }
            } else {
                break;
            }
        }

        let success = run_single_check(&probe, &working_dir, &environment).await;

        if success {
            consecutive_successes += 1;
            consecutive_failures = 0;
            if consecutive_successes >= probe.success_threshold {
                let mut guard = state.lock().await;
                if let Some(runtime) = guard.processes.get_mut(&name) {
                    runtime.healthy = true;
                }
            }
        } else {
            consecutive_failures += 1;
            consecutive_successes = 0;
            if consecutive_failures >= probe.failure_threshold {
                let mut guard = state.lock().await;
                if let Some(runtime) = guard.processes.get_mut(&name) {
                    runtime.healthy = false;
                }
            }
        }

        sleep(Duration::from_secs(probe.period_seconds)).await;
    }
}

/// Run a liveness probe periodically. When the failure threshold is reached,
/// the process is killed so that the restart policy can re-launch it.
async fn run_liveness_probe(
    name: String,
    probe: HealthProbe,
    state: SharedState,
    working_dir: std::path::PathBuf,
    environment: BTreeMap<String, String>,
) {
    // Initial delay
    if probe.initial_delay_seconds > 0 {
        sleep(Duration::from_secs(probe.initial_delay_seconds)).await;
    }

    let mut consecutive_failures: u32 = 0;

    loop {
        // Check if process is still running
        {
            let guard = state.lock().await;
            if let Some(runtime) = guard.processes.get(&name) {
                if runtime.status.is_terminal() {
                    break;
                }
            } else {
                break;
            }
        }

        let success = run_single_check(&probe, &working_dir, &environment).await;

        if success {
            consecutive_failures = 0;
        } else {
            consecutive_failures += 1;
            if consecutive_failures >= probe.failure_threshold {
                // Kill the process so the restart policy can re-launch it.
                let guard = state.lock().await;
                if let Some(runtime) = guard.processes.get(&name) {
                    if let ProcessStatus::Running { pid } = runtime.status {
                        #[cfg(unix)]
                        {
                            use nix::sys::signal::{self, Signal};
                            use nix::unistd::Pid;
                            let _ = signal::kill(Pid::from_raw(pid as i32), Signal::SIGKILL);
                        }
                        #[cfg(not(unix))]
                        {
                            let _ = pid; // suppress unused warning
                        }
                    }
                }
                drop(guard);

                // Reset counter — the restart policy will re-launch the
                // process and the probe will re-evaluate from scratch.
                consecutive_failures = 0;

                // Wait for the process to actually restart before probing
                // again so we don't immediately kill the new instance.
                sleep(Duration::from_secs(probe.period_seconds)).await;
                continue;
            }
        }

        sleep(Duration::from_secs(probe.period_seconds)).await;
    }
}

async fn run_single_check(
    probe: &HealthProbe,
    working_dir: &Path,
    environment: &BTreeMap<String, String>,
) -> bool {
    if let Some(ref exec) = probe.exec {
        let timeout = Duration::from_secs(probe.timeout_seconds);
        let mut cmd = match build_shell_command(&exec.command) {
            Ok(c) => c,
            Err(_) => return false,
        };
        cmd.current_dir(working_dir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .envs(environment);
        match tokio::time::timeout(timeout, cmd.output()).await {
            Ok(Ok(output)) => return output.status.success(),
            _ => return false,
        }
    }

    if let Some(ref http) = probe.http_get {
        let timeout = Duration::from_secs(probe.timeout_seconds);
        return tokio::time::timeout(timeout, http_get_check(http))
            .await
            .unwrap_or(false);
    }

    false
}

async fn http_get_check(http: &crate::model::HttpCheck) -> bool {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::TcpStream;

    let addr = format!("{}:{}", http.host, http.port);
    let mut stream = match TcpStream::connect(&addr).await {
        Ok(s) => s,
        Err(_) => return false,
    };

    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {}:{}\r\nConnection: close\r\n\r\n",
        http.path, http.host, http.port
    );
    if stream.write_all(request.as_bytes()).await.is_err() {
        return false;
    }

    let mut buf = vec![0u8; 1024];
    let n = match stream.read(&mut buf).await {
        Ok(n) if n > 0 => n,
        _ => return false,
    };

    // Parse status code from "HTTP/1.x NNN ..."
    let response = String::from_utf8_lossy(&buf[..n]);
    let status: u16 = response
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    (200..400).contains(&status)
}

/// Build a shell command for executing a user-supplied command string.
///
/// Commands come from user-authored config files (trusted input), matching
/// Docker Compose semantics where `command:` is always interpreted by a shell.
/// The shell is configurable via the `COMPOSE_SHELL` env var (default: `sh`).
fn build_shell_command(command: &str) -> Result<TokioCommand> {
    if cfg!(windows) {
        let mut cmd = TokioCommand::new("cmd");
        cmd.arg("/C").arg(command);
        Ok(cmd)
    } else {
        let shell = env::var("COMPOSE_SHELL").unwrap_or_else(|_| "sh".to_string());

        // Validate the shell exists and is plausibly executable.
        let shell_path = Path::new(&shell);
        if shell_path.is_absolute() {
            if !shell_path.exists() {
                anyhow::bail!("shell {shell:?} (from COMPOSE_SHELL) does not exist");
            }
        } else {
            // For bare names, verify they can be found on PATH.
            if which::which(&shell).is_err() {
                anyhow::bail!("shell {shell:?} (from COMPOSE_SHELL) not found on PATH");
            }
        }

        let mut cmd = TokioCommand::new(&shell);
        cmd.arg("-c").arg(command);
        Ok(cmd)
    }
}

/// Resolve a list of service names (base names or replica-qualified names)
/// into the set of runtime instance names in the daemon's process map.
///
/// If `services` is empty, returns all runtime names (sorted).
/// If any service name doesn't match at least one runtime, returns Err with
/// the list of unknown names.
fn resolve_services(
    state: &DaemonState,
    services: &[String],
) -> std::result::Result<Vec<String>, Vec<String>> {
    if services.is_empty() {
        return Ok(state.processes.keys().cloned().collect());
    }

    let mut unknown = Vec::new();
    let mut matched = std::collections::BTreeSet::new();
    for svc in services {
        let mut found = false;
        for (name, runtime) in &state.processes {
            if runtime.spec.base_name == *svc || runtime.spec.name == *svc {
                matched.insert(name.clone());
                found = true;
            }
        }
        if !found {
            unknown.push(svc.clone());
        }
    }

    if unknown.is_empty() {
        Ok(matched.into_iter().collect())
    } else {
        Err(unknown)
    }
}

fn describe_services(services: &[String]) -> String {
    if services.is_empty() {
        "all services".to_string()
    } else {
        services.join(", ")
    }
}

async fn handle_client(stream: Stream, state: SharedState) -> Result<()> {
    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    let read = reader
        .read_line(&mut line)
        .await
        .context("failed to read request line")?;
    if read == 0 {
        return Ok(());
    }

    let req: Request = serde_json::from_str(line.trim()).context("invalid request json")?;
    let response = match req {
        Request::Ping => {
            let guard = state.lock().await;
            Response::Pong {
                pid: std::process::id(),
                instance: guard.instance.clone(),
            }
        }
        Request::Ps => {
            let guard = state.lock().await;
            let processes = guard
                .processes
                .values()
                .map(|proc_runtime| {
                    let exit_code = match &proc_runtime.status {
                        ProcessStatus::Exited { code } => Some(*code),
                        _ => None,
                    };
                    let pid = match &proc_runtime.status {
                        ProcessStatus::Running { pid } => Some(*pid),
                        _ => None,
                    };
                    ProcessSnapshot {
                        name: proc_runtime.spec.name.clone(),
                        base: proc_runtime.spec.base_name.clone(),
                        replica: proc_runtime.spec.replica,
                        status: proc_runtime.status.to_human(),
                        state: proc_runtime.status.to_json_status().to_string(),
                        description: proc_runtime.spec.description.clone(),
                        restart_count: proc_runtime.restart_count,
                        log_ready: proc_runtime.log_ready,
                        healthy: proc_runtime.healthy,
                        has_readiness_probe: proc_runtime.spec.readiness_probe.is_some(),
                        pid,
                        exit_code,
                    }
                })
                .collect::<Vec<_>>();

            Response::Ps {
                pid: std::process::id(),
                instance: guard.instance.clone(),
                processes,
            }
        }
        Request::Down { timeout_seconds } => {
            let mut guard = state.lock().await;
            guard.shutdown_requested = true;
            guard.shutdown_timeout_override = timeout_seconds;
            for tx in guard.controllers.values() {
                let _ = tx.send(true);
            }
            // Pending processes have no controller — stop them directly
            for runtime in guard.processes.values_mut() {
                if matches!(runtime.status, ProcessStatus::Pending) {
                    runtime.status = ProcessStatus::Stopped;
                }
            }
            Response::Ack {
                message: "shutdown requested".to_string(),
            }
        }
        Request::Stop { services } => {
            let mut guard = state.lock().await;
            match resolve_services(&guard, &services) {
                Err(unknown) => Response::Error {
                    message: format!("unknown service(s): {}", unknown.join(", ")),
                },
                Ok(names) => {
                    for name in &names {
                        // Processes in Pending state have no controller yet —
                        // transition them directly to Stopped.
                        if let Some(runtime) = guard.processes.get(name) {
                            if matches!(runtime.status, ProcessStatus::Pending) {
                                guard.processes.get_mut(name).unwrap().status =
                                    ProcessStatus::Stopped;
                                continue;
                            }
                        }
                        if let Some(tx) = guard.controllers.get(name) {
                            let _ = tx.send(true);
                        }
                    }
                    Response::Ack {
                        message: format!("stopping {}", describe_services(&services)),
                    }
                }
            }
        }
        Request::Start { services } => {
            let mut guard = state.lock().await;
            match resolve_services(&guard, &services) {
                Err(unknown) => Response::Error {
                    message: format!("unknown service(s): {}", unknown.join(", ")),
                },
                Ok(names) => {
                    // Collect the set of services to start, including their
                    // transitive dependencies that are also in a terminal state
                    // (e.g. NotStarted). This ensures `start serviceB` also
                    // brings up serviceB's deps if they haven't been launched.
                    let mut to_start: std::collections::BTreeSet<String> =
                        names.into_iter().collect();
                    let mut queue: std::collections::VecDeque<String> =
                        to_start.iter().cloned().collect();
                    while let Some(name) = queue.pop_front() {
                        if let Some(runtime) = guard.processes.get(&name) {
                            for dep_base in runtime.spec.depends_on.keys() {
                                // Find all runtime instances matching the dep base name
                                let dep_names: Vec<String> = guard
                                    .processes
                                    .iter()
                                    .filter(|(_, r)| r.spec.base_name == *dep_base)
                                    .map(|(n, _)| n.clone())
                                    .collect();
                                for dep_name in dep_names {
                                    if to_start.insert(dep_name.clone()) {
                                        queue.push_back(dep_name);
                                    }
                                }
                            }
                        }
                    }

                    let mut started = 0;
                    for name in &to_start {
                        if let Some(runtime) = guard.processes.get_mut(name) {
                            if runtime.status.is_terminal() {
                                runtime.status = ProcessStatus::Pending;
                                runtime.log_ready = false;
                                runtime.healthy = false;
                                started += 1;
                            }
                        }
                    }
                    if started == 0 {
                        Response::Ack {
                            message: format!("{} already running", describe_services(&services)),
                        }
                    } else {
                        Response::Ack {
                            message: format!("starting {}", describe_services(&services)),
                        }
                    }
                }
            }
        }
        Request::Kill { services, signal } => {
            let guard = state.lock().await;
            match resolve_services(&guard, &services) {
                Err(unknown) => Response::Error {
                    message: format!("unknown service(s): {}", unknown.join(", ")),
                },
                Ok(names) => {
                    for name in &names {
                        if let Some(runtime) = guard.processes.get(name) {
                            if let ProcessStatus::Running { pid } = runtime.status {
                                #[cfg(unix)]
                                {
                                    use nix::sys::signal::{self, Signal};
                                    use nix::unistd::Pid;
                                    if let Ok(sig) = Signal::try_from(signal) {
                                        let _ = signal::kill(Pid::from_raw(pid as i32), sig);
                                    }
                                }
                            }
                        }
                    }
                    Response::Ack {
                        message: format!("killed {}", describe_services(&services)),
                    }
                }
            }
        }
        Request::Restart { services } => {
            let guard = state.lock().await;
            match resolve_services(&guard, &services) {
                Err(unknown) => Response::Error {
                    message: format!("unknown service(s): {}", unknown.join(", ")),
                },
                Ok(names) => {
                    for name in &names {
                        if let Some(tx) = guard.controllers.get(name) {
                            let _ = tx.send(true);
                        }
                    }
                    drop(guard);
                    // Spawn a task to wait for stop then reset to Pending
                    let state_clone = state.clone();
                    let names_clone = names.clone();
                    tokio::spawn(async move {
                        for _ in 0..200 {
                            let all_stopped = {
                                let guard = state_clone.lock().await;
                                names_clone.iter().all(|name| {
                                    guard
                                        .processes
                                        .get(name)
                                        .map(|r| r.status.is_terminal())
                                        .unwrap_or(true)
                                })
                            };
                            if all_stopped {
                                break;
                            }
                            sleep(Duration::from_millis(50)).await;
                        }
                        let mut guard = state_clone.lock().await;
                        for name in &names_clone {
                            if let Some(runtime) = guard.processes.get_mut(name) {
                                runtime.status = ProcessStatus::Pending;
                                runtime.log_ready = false;
                                runtime.healthy = false;
                            }
                        }
                    });
                    Response::Ack {
                        message: format!("restarting {}", describe_services(&services)),
                    }
                }
            }
        }
        Request::RemoveOrphans { keep } => {
            let mut guard = state.lock().await;
            let keep_set: std::collections::HashSet<&str> =
                keep.iter().map(|s| s.as_str()).collect();
            let orphans: Vec<String> = guard
                .processes
                .keys()
                .filter(|name| {
                    let base = guard.processes[name.as_str()].spec.base_name.as_str();
                    !keep_set.contains(base)
                })
                .cloned()
                .collect();
            for name in &orphans {
                if let Some(runtime) = guard.processes.get(name) {
                    if matches!(runtime.status, ProcessStatus::Pending) {
                        guard.processes.get_mut(name).unwrap().status = ProcessStatus::Stopped;
                        continue;
                    }
                }
                if let Some(tx) = guard.controllers.get(name) {
                    let _ = tx.send(true);
                }
            }
            // Spawn a task to wait for orphans to stop then remove them
            if !orphans.is_empty() {
                let state_clone = state.clone();
                let orphans_clone = orphans.clone();
                tokio::spawn(async move {
                    for _ in 0..200 {
                        let all_stopped = {
                            let guard = state_clone.lock().await;
                            orphans_clone.iter().all(|name| {
                                guard
                                    .processes
                                    .get(name)
                                    .map(|r| r.status.is_terminal())
                                    .unwrap_or(true)
                            })
                        };
                        if all_stopped {
                            break;
                        }
                        sleep(Duration::from_millis(50)).await;
                    }
                    let mut guard = state_clone.lock().await;
                    for name in &orphans_clone {
                        guard.processes.remove(name);
                        guard.controllers.remove(name);
                    }
                });
            }
            let msg = if orphans.is_empty() {
                "no orphan processes found".to_string()
            } else {
                format!("removing orphan(s): {}", orphans.join(", "))
            };
            Response::Ack { message: msg }
        }
    };

    let payload = serde_json::to_string(&response)?;
    write_half.write_all(payload.as_bytes()).await?;
    write_half.write_all(b"\n").await?;
    write_half.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use super::*;
    use crate::model::{
        DependencyCondition, ProcessInstanceSpec, ProcessRuntime, ProcessStatus, RestartPolicy,
    };

    fn runtime_with(base: &str, status: ProcessStatus, started_once: bool) -> ProcessRuntime {
        ProcessRuntime {
            spec: ProcessInstanceSpec {
                name: base.to_string(),
                base_name: base.to_string(),
                replica: 1,
                command: "echo".to_string(),
                description: None,
                working_dir: PathBuf::from("/tmp"),
                environment: BTreeMap::new(),
                depends_on: BTreeMap::new(),
                ready_log_line: None,
                restart_policy: RestartPolicy::No,
                backoff_seconds: 1,
                max_restarts: None,
                shutdown_signal: None,
                shutdown_timeout_seconds: 10,
                shutdown_command: None,
                readiness_probe: None,
                liveness_probe: None,
                disabled: false,
            },
            status,
            started_once,
            log_ready: false,
            restart_count: 0,
            healthy: false,
        }
    }

    #[test]
    fn dependencies_require_all_replicas_to_satisfy_condition() {
        let mut snapshot = BTreeMap::new();
        snapshot.insert("db[1]".to_string(), {
            let mut r = runtime_with("db", ProcessStatus::Exited { code: 0 }, true);
            r.spec.name = "db[1]".to_string();
            r.spec.replica = 1;
            r
        });
        snapshot.insert("db[2]".to_string(), {
            let mut r = runtime_with("db", ProcessStatus::Exited { code: 1 }, true);
            r.spec.name = "db[2]".to_string();
            r.spec.replica = 2;
            r
        });

        let mut candidate = runtime_with("api", ProcessStatus::Pending, false);
        candidate.spec.depends_on.insert(
            "db".to_string(),
            DependencyCondition::ProcessCompletedSuccessfully,
        );
        assert!(!dependencies_met(&candidate, &snapshot));

        if let Some(db2) = snapshot.get_mut("db[2]") {
            db2.status = ProcessStatus::Exited { code: 0 };
        }
        assert!(dependencies_met(&candidate, &snapshot));
    }

    #[test]
    fn process_started_condition_uses_started_once() {
        let mut snapshot = BTreeMap::new();
        snapshot.insert(
            "db".to_string(),
            runtime_with("db", ProcessStatus::Pending, false),
        );

        let mut candidate = runtime_with("api", ProcessStatus::Pending, false);
        candidate
            .spec
            .depends_on
            .insert("db".to_string(), DependencyCondition::ProcessStarted);
        assert!(!dependencies_met(&candidate, &snapshot));

        if let Some(db) = snapshot.get_mut("db") {
            db.started_once = true;
            db.status = ProcessStatus::Running { pid: 42 };
        }
        assert!(dependencies_met(&candidate, &snapshot));
    }

    #[test]
    fn process_log_ready_condition_uses_log_ready_flag() {
        let mut snapshot = BTreeMap::new();
        let mut db = runtime_with("db", ProcessStatus::Running { pid: 42 }, true);
        db.log_ready = false;
        snapshot.insert("db".to_string(), db);

        let mut candidate = runtime_with("api", ProcessStatus::Pending, false);
        candidate
            .spec
            .depends_on
            .insert("db".to_string(), DependencyCondition::ProcessLogReady);
        assert!(!dependencies_met(&candidate, &snapshot));

        if let Some(db) = snapshot.get_mut("db") {
            db.log_ready = true;
        }
        assert!(dependencies_met(&candidate, &snapshot));
    }
}
