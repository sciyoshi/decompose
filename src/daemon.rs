use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::fs::OpenOptions;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
#[cfg(unix)]
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

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
#[cfg(unix)]
use crate::paths::FILE_MODE;
use crate::paths::{create_dir_secure, runtime_paths_for};

/// Compile a `ready_log_line` pattern, falling back to a literal (escaped)
/// match if the user-supplied pattern isn't a valid regex. The escaped
/// fallback is guaranteed-valid regex, so this never panics.
fn compile_ready_pattern(pattern: &str) -> Regex {
    Regex::new(pattern).unwrap_or_else(|_| {
        // `regex::escape` returns a string of literal characters only, which
        // is always a valid regex. If this somehow fails, fall back to a
        // never-matching pattern rather than panicking.
        Regex::new(&regex::escape(pattern))
            .unwrap_or_else(|_| Regex::new("$.^").expect("never-matching pattern is valid regex"))
    })
}

/// Open a file with restrictive 0o600 permissions on Unix, and defensively
/// tighten the mode if the file already existed with looser permissions.
#[cfg(unix)]
fn open_secure_append(path: &Path) -> std::io::Result<fs::File> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(FILE_MODE)
        .open(path)?;
    // If the file existed before this call, `mode` is ignored and we must
    // tighten it explicitly.
    fs::set_permissions(path, fs::Permissions::from_mode(FILE_MODE))?;
    Ok(file)
}

#[cfg(not(unix))]
fn open_secure_append(path: &Path) -> std::io::Result<fs::File> {
    OpenOptions::new().create(true).append(true).open(path)
}

#[cfg(unix)]
fn open_secure_lock(path: &Path) -> std::io::Result<fs::File> {
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .mode(FILE_MODE)
        .open(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(FILE_MODE))?;
    Ok(file)
}

#[cfg(not(unix))]
fn open_secure_lock(path: &Path) -> std::io::Result<fs::File> {
    OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(path)
}

/// Write a file atomically-ish with 0o600 permissions.
#[cfg(unix)]
fn write_secure(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(FILE_MODE)
        .open(path)?;
    file.write_all(contents)?;
    fs::set_permissions(path, fs::Permissions::from_mode(FILE_MODE))?;
    Ok(())
}

#[cfg(not(unix))]
fn write_secure(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    fs::write(path, contents)
}

#[derive(Debug)]
struct DaemonState {
    instance: String,
    processes: BTreeMap<String, ProcessRuntime>,
    controllers: BTreeMap<String, watch::Sender<bool>>,
    shutdown_requested: bool,
    exit_mode: ExitMode,
    /// CLI-level override for shutdown timeout (from `down --timeout`).
    shutdown_timeout_override: Option<u64>,
    /// Original daemon-launch arguments kept so the `Reload` IPC handler can
    /// re-read config from the same paths the daemon was launched with.
    cwd: std::path::PathBuf,
    config_files: Vec<std::path::PathBuf>,
    env_files: Vec<std::path::PathBuf>,
    disable_dotenv: bool,
    /// Timestamp of the most recent IPC request. The orphan watchdog uses
    /// this to decide whether the daemon still has active clients talking to
    /// it after its parent process exits. Seeded at daemon start, then
    /// updated at the top of every `handle_client` call.
    last_client_activity: Instant,
}

impl DaemonState {
    /// Broadcast a shutdown signal to every running process controller and
    /// transition any still-Pending processes directly to Stopped (they have
    /// no controller of their own). Does not set `shutdown_requested`.
    fn broadcast_stop(&mut self) {
        for tx in self.controllers.values() {
            let _ = tx.send(true);
        }
        for runtime in self.processes.values_mut() {
            if matches!(runtime.status, ProcessStatus::Pending) {
                runtime.status = ProcessStatus::Stopped;
            }
        }
    }

    /// Set `shutdown_requested` and broadcast stop to all controllers. Used
    /// by callers that want to initiate shutdown (exit-mode trigger, fatal
    /// accept error, `Down` RPC).
    fn request_shutdown(&mut self) {
        self.shutdown_requested = true;
        self.broadcast_stop();
    }

    /// Stop a specific set of process instances by name. For each name:
    /// - If the process is `Pending` (has no controller yet), transition it
    ///   directly to `Stopped`.
    /// - Otherwise, send the shutdown signal to its controller so the
    ///   lifecycle task will tear it down.
    ///
    /// Unknown names are silently ignored (callers should resolve/validate
    /// first). Used by the Stop, RemoveOrphans, and Reload IPC handlers,
    /// which all share this "best-effort targeted shutdown" shape.
    fn stop_instances(&mut self, names: &[String]) {
        for name in names {
            if let Some(runtime) = self.processes.get_mut(name) {
                if matches!(runtime.status, ProcessStatus::Pending) {
                    runtime.status = ProcessStatus::Stopped;
                    continue;
                }
            }
            if let Some(tx) = self.controllers.get(name) {
                let _ = tx.send(true);
            }
        }
    }
}

/// Poll the shared state until every instance in `names` has reached a
/// terminal status, or until `max_ticks` of `tick` elapses. Returns once
/// either condition holds. Callers use this to gate follow-up work (e.g.
/// respawning or removing entries) on the preceding stop signal actually
/// having landed.
async fn wait_for_terminal(state: &SharedState, names: &[String], tick: Duration, max_ticks: u32) {
    for _ in 0..max_ticks {
        let all_stopped = {
            let guard = state.lock().await;
            names.iter().all(|name| {
                guard
                    .processes
                    .get(name)
                    .map(|r| r.status.is_terminal())
                    .unwrap_or(true)
            })
        };
        if all_stopped {
            return;
        }
        sleep(tick).await;
    }
}

type SharedState = Arc<Mutex<DaemonState>>;

/// Resolve `handle` to the current name, take the state mutex, and run `f`
/// against the matching [`ProcessRuntime`] if one exists. Returns the closure's
/// result, or `None` if the runtime entry is gone (e.g. after a rename).
///
/// This is a thin wrapper over the lock→resolve→`get_mut` pattern repeated
/// throughout the daemon; it exists purely to cut down on ceremony at call
/// sites. Behaviour matches the hand-written form exactly.
async fn with_process_mut<F, R>(
    state: &SharedState,
    handle: &crate::model::NameHandle,
    f: F,
) -> Option<R>
where
    F: FnOnce(&mut ProcessRuntime) -> R,
{
    let name = crate::model::read_name(handle);
    let mut guard = state.lock().await;
    guard.processes.get_mut(&name).map(f)
}

/// Read-only counterpart to [`with_process_mut`]: resolves the name, takes the
/// lock for reading, and runs `f` against `&ProcessRuntime`. Returns `None` if
/// the entry is gone.
async fn with_process<F, R>(
    state: &SharedState,
    handle: &crate::model::NameHandle,
    f: F,
) -> Option<R>
where
    F: FnOnce(&ProcessRuntime) -> R,
{
    let name = crate::model::read_name(handle);
    let guard = state.lock().await;
    guard.processes.get(&name).map(f)
}

/// Acquire an exclusive, non-blocking advisory lock for this daemon instance.
///
/// Returns the open lock file handle — the caller must keep it alive for the
/// daemon's entire lifetime. The lock is automatically released when the
/// file descriptor is closed (including on crash).
#[cfg(unix)]
fn acquire_instance_lock(paths: &RuntimePaths) -> Result<fs::File> {
    let lock_file = open_secure_lock(&paths.lock)
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
    open_secure_lock(&paths.lock)
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
    parent_pid: Option<u32>,
) -> Result<()> {
    let exe = env::current_exe().context("failed to locate current executable")?;
    if let Some(parent) = paths.daemon_log.parent() {
        create_dir_secure(parent)?;
    }

    let mut log_file = open_secure_append(&paths.daemon_log).with_context(|| {
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

    if let Some(pid) = parent_pid {
        cmd.arg("--parent-pid").arg(pid.to_string());
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
        create_dir_secure(parent)?;
    }
    if let Some(parent) = paths.pid.parent() {
        create_dir_secure(parent)?;
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

    write_secure(&paths.pid, std::process::id().to_string().as_bytes()).with_context(|| {
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

    // Tighten the newly-created socket's file mode. The `interprocess` crate
    // binds the socket with the current umask, which could leave it world-
    // readable or world-writable. We want owner-only access on local sockets.
    #[cfg(unix)]
    if paths.socket.exists() {
        let _ = fs::set_permissions(&paths.socket, fs::Permissions::from_mode(FILE_MODE));
    }

    let state = Arc::new(Mutex::new(DaemonState {
        instance: args.instance.clone(),
        processes: process_map,
        controllers: BTreeMap::new(),
        shutdown_requested: false,
        exit_mode,
        shutdown_timeout_override: None,
        cwd: args.cwd.clone(),
        config_files: args.config_files.clone(),
        env_files: args.env_files.clone(),
        disable_dotenv: args.disable_dotenv,
        last_client_activity: Instant::now(),
    }));

    let (stop_tx, mut stop_rx) = watch::channel(false);
    tokio::spawn(supervisor_loop(state.clone(), stop_tx));

    // Orphan watchdog: when the launching process goes away without calling
    // `down`, auto-exit after a grace period of no IPC activity. Inert when
    // no parent PID was passed (i.e. `up -d`, where the daemon is intended
    // to survive its caller).
    if let Some(parent_pid) = args.parent_pid {
        tokio::spawn(orphan_watchdog(state.clone(), parent_pid));
    }

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
                        // Accept errors are almost always transient (e.g.
                        // ECONNABORTED when a client hangs up between the
                        // kernel accepting the connection and us reading it,
                        // or EMFILE if we're briefly out of file descriptors).
                        // We distinguish "fatal" cases — principally EBADF,
                        // which would indicate the listening socket has been
                        // closed out from under us — and shut the daemon
                        // down. Everything else we log and retry after a
                        // short backoff.
                        #[cfg(unix)]
                        let is_fatal = matches!(e.raw_os_error(), Some(libc::EBADF) | Some(libc::ENOTSOCK) | Some(libc::EINVAL));
                        #[cfg(not(unix))]
                        let is_fatal = false;

                        if is_fatal {
                            eprintln!("socket accept failed fatally, shutting down: {e}");
                            // Trigger a graceful shutdown — supervisor will
                            // stop processes and break the outer loop.
                            let mut guard = state.lock().await;
                            guard.request_shutdown();
                            break;
                        } else {
                            eprintln!("socket accept error (transient): {e}");
                            sleep(Duration::from_millis(50)).await;
                        }
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
                    guard.request_shutdown();
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
                        guard.broadcast_stop();
                    }
                }
            } else {
                request_shutdown = true;
                guard.broadcast_stop();
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

        sleep(crate::tuning::supervisor_tick()).await;
    }
}

/// Return `true` if the given PID is still alive (or we can't tell).
/// On Unix, uses `kill(pid, 0)`: returns `Ok` if the process exists or we
/// lack permission to signal it (still alive), and `ESRCH` if the PID is
/// unused. PID 0 and 1 are treated as "alive" (PID 1 is init/launchd, which
/// `getppid()` returns on macOS after the real parent exits — we detect
/// orphan state via the caller-supplied PID, not `getppid`).
#[cfg(unix)]
fn parent_alive(pid: u32) -> bool {
    if pid == 0 {
        return true;
    }
    use nix::errno::Errno;
    use nix::sys::signal::kill;
    use nix::unistd::Pid;
    match kill(Pid::from_raw(pid as i32), None) {
        Ok(()) => true,
        Err(Errno::ESRCH) => false,
        // EPERM means the process exists but we can't signal it.
        Err(_) => true,
    }
}

#[cfg(not(unix))]
fn parent_alive(_pid: u32) -> bool {
    // Best-effort: on non-Unix platforms we skip the check entirely.
    true
}

/// Periodically check whether the caller that launched this daemon is still
/// alive. Once the parent PID is gone AND no IPC client has spoken to us in
/// the configured grace period, initiate a graceful shutdown. The grace
/// period lets transient tools (`ps`, `logs`, `start`) keep a daemon alive
/// even after the original terminal disappeared — only a truly abandoned
/// daemon self-exits.
async fn orphan_watchdog(state: SharedState, parent_pid: u32) {
    let tick = crate::tuning::orphan_check_interval();
    let grace = crate::tuning::orphan_timeout();
    loop {
        sleep(tick).await;

        {
            let guard = state.lock().await;
            if guard.shutdown_requested {
                return;
            }
        }

        if parent_alive(parent_pid) {
            continue;
        }

        // Parent is gone. Check whether any client has been in touch
        // recently; if so, defer.
        let should_exit = {
            let guard = state.lock().await;
            guard.last_client_activity.elapsed() >= grace
        };

        if should_exit {
            eprintln!(
                "daemon: parent pid {parent_pid} is gone and no IPC activity for \
                 {}s; initiating shutdown",
                grace.as_secs()
            );
            let mut guard = state.lock().await;
            guard.request_shutdown();
            return;
        }
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
            // `process_healthy` gates on readiness (the readiness probe's
            // pass/fail state). Liveness intentionally does not participate:
            // a process with no readiness probe configured can never satisfy
            // this condition — use `process_started` or `process_log_ready`
            // when that's the desired semantics. See model.rs for the
            // `ready`/`alive` split.
            DependencyCondition::ProcessHealthy => dep_instances.iter().all(|p| p.ready),
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
    // Pull the spec and the name handle for a pending process. Non-pending
    // (or missing) entries are silent no-ops — callers may fire
    // start_process opportunistically.
    let (spec, name_handle) = {
        let mut guard = state.lock().await;
        let Some(runtime) = guard.processes.get_mut(&name) else {
            return;
        };
        if !matches!(runtime.status, ProcessStatus::Pending) {
            return;
        }
        (runtime.spec.clone(), runtime.name_handle.clone())
    };

    let ready_pattern: Option<Regex> = spec.ready_log_line.as_deref().map(compile_ready_pattern);

    let Some(mut child) =
        spawn_process_child(&name_handle, &spec, &state, SpawnContext::Initial).await
    else {
        return;
    };

    let pid = child.id().unwrap_or(0);
    with_process_mut(&state, &name_handle, |runtime| {
        runtime.status = ProcessStatus::Running { pid };
        runtime.started_once = true;
    })
    .await;

    spawn_health_probes(&name_handle, &spec, &state);
    attach_output_readers(&mut child, &name_handle, ready_pattern, state.clone());

    let (kill_tx, kill_rx) = watch::channel(false);
    {
        let current = crate::model::read_name(&name_handle);
        let mut guard = state.lock().await;
        guard.controllers.insert(current, kill_tx);
    }

    tokio::spawn(process_lifecycle(
        name_handle,
        spec,
        child,
        kill_rx,
        state.clone(),
    ));
}

/// Identifies whether we're doing the initial spawn or a restart attempt,
/// for shaping the error-context messages emitted on spawn failure.
#[derive(Clone, Copy)]
enum SpawnContext {
    Initial,
    Restart,
}

impl SpawnContext {
    fn build_label(self) -> &'static str {
        match self {
            SpawnContext::Initial => "failed to build shell command",
            SpawnContext::Restart => "failed to build shell command on restart",
        }
    }

    fn spawn_label(self) -> &'static str {
        match self {
            SpawnContext::Initial => "failed to spawn process",
            SpawnContext::Restart => "failed to spawn process on restart",
        }
    }
}

/// Build a shell command from `spec`, apply the common stdio/env setup, and
/// spawn the child. On failure, log the error, transition the process to
/// `FailedToStart`, and return `None`. The `ctx` parameter only affects the
/// phrasing of error messages (initial spawn vs. restart).
async fn spawn_process_child(
    name_handle: &crate::model::NameHandle,
    spec: &crate::model::ProcessInstanceSpec,
    state: &SharedState,
    ctx: SpawnContext,
) -> Option<tokio::process::Child> {
    let name = crate::model::read_name(name_handle);
    let mut cmd = match build_shell_command(&spec.command)
        .with_context(|| format!("[{name}] {} for {:?}", ctx.build_label(), spec.command))
    {
        Ok(c) => c,
        Err(e) => {
            mark_failed_to_start(name_handle, state, &e).await;
            return None;
        }
    };
    cmd.current_dir(&spec.working_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .envs(&spec.environment);

    match cmd.spawn().with_context(|| {
        format!(
            "[{name}] {} (command={:?}, cwd={})",
            ctx.spawn_label(),
            spec.command,
            spec.working_dir.display()
        )
    }) {
        Ok(child) => Some(child),
        Err(e) => {
            mark_failed_to_start(name_handle, state, &e).await;
            None
        }
    }
}

/// Log the (already-contextualised) spawn error and transition the process
/// to `FailedToStart`. Does not remove the controller — the caller decides
/// what cleanup is appropriate for its phase.
async fn mark_failed_to_start(
    name_handle: &crate::model::NameHandle,
    state: &SharedState,
    err: &anyhow::Error,
) {
    let name = crate::model::read_name(name_handle);
    eprintln!("[{name}] {err:#}");
    with_process_mut(state, name_handle, |runtime| {
        runtime.status = ProcessStatus::FailedToStart {
            reason: format!("{err:#}"),
        };
    })
    .await;
}

/// Spawn readiness and liveness probe tasks if configured on the spec.
fn spawn_health_probes(
    name_handle: &crate::model::NameHandle,
    spec: &crate::model::ProcessInstanceSpec,
    state: &SharedState,
) {
    spawn_probe_if_present(
        spec.readiness_probe.as_ref(),
        ProbeKind::Readiness,
        name_handle,
        &spec.working_dir,
        &spec.environment,
        state,
    );
    spawn_probe_if_present(
        spec.liveness_probe.as_ref(),
        ProbeKind::Liveness,
        name_handle,
        &spec.working_dir,
        &spec.environment,
        state,
    );
}

/// How a child process ended, carrying enough information to render a
/// human-readable restart separator. `Code` is the normal-exit path;
/// `Signal` is a Unix signal (or `-1` exit code when the platform didn't
/// provide one).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExitReason {
    Code(i32),
    Signal(i32),
}

impl ExitReason {
    fn describe(self) -> String {
        match self {
            ExitReason::Code(code) => format!("exit code {code}"),
            ExitReason::Signal(sig) => {
                let name = signal_name(sig);
                match name {
                    Some(n) => format!("signal {n}"),
                    None => format!("signal {sig}"),
                }
            }
        }
    }
}

/// Return the canonical `SIG*` name for a signal number, or `None` if
/// `nix` doesn't know about it. Keeps the restart separator readable
/// without requiring a full libc signal table.
fn signal_name(sig: i32) -> Option<&'static str> {
    use nix::sys::signal::Signal;
    Signal::try_from(sig).ok().map(|s| s.as_str())
}

/// Wait for either a kill signal or the child to exit, returning the
/// resulting terminal [`ProcessStatus`] along with an [`ExitReason`]
/// describing the child's outcome (used for the restart separator).
/// On kill, this also runs the spec-driven shutdown sequence (command,
/// signal, SIGKILL).
async fn wait_for_child_exit(
    name_handle: &crate::model::NameHandle,
    spec: &crate::model::ProcessInstanceSpec,
    child: &mut tokio::process::Child,
    kill_rx: &mut watch::Receiver<bool>,
    state: &SharedState,
) -> (ProcessStatus, Option<ExitReason>) {
    tokio::select! {
        _ = kill_rx.changed() => {
            let timeout_override = {
                let guard = state.lock().await;
                guard.shutdown_timeout_override
            };
            shutdown_child(child, spec, timeout_override).await;
            let _ = name_handle; // handle retained for future logging hooks
            (ProcessStatus::Stopped, None)
        }
        wait_res = child.wait() => {
            match wait_res {
                Ok(exit_status) => {
                    let reason = exit_reason_from_status(&exit_status);
                    let status = match reason {
                        ExitReason::Code(code) => ProcessStatus::Exited { code },
                        // ProcessStatus doesn't distinguish signal vs. code
                        // today; collapse to `code=-1` matching prior
                        // behavior, but keep the richer reason for logging.
                        ExitReason::Signal(_) => ProcessStatus::Exited { code: -1 },
                    };
                    (status, Some(reason))
                }
                Err(e) => (
                    ProcessStatus::FailedToStart {
                        reason: format!("wait failed: {e}"),
                    },
                    None,
                ),
            }
        }
    }
}

/// Map a child `ExitStatus` into an [`ExitReason`], preferring the
/// numeric exit code when available and falling back to the signal
/// number on Unix.
fn exit_reason_from_status(status: &std::process::ExitStatus) -> ExitReason {
    if let Some(code) = status.code() {
        return ExitReason::Code(code);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            return ExitReason::Signal(sig);
        }
    }
    ExitReason::Code(-1)
}

/// Format the separator line that is written to the daemon log between
/// consecutive runs of a process. The line is already prefixed with
/// `[name]` so that `decompose logs <name>` filtering picks it up,
/// matching the format the stdout/stderr readers emit.
fn format_restart_separator(
    name: &str,
    reason: ExitReason,
    attempt: u32,
    max: Option<u32>,
) -> String {
    let attempt_part = match max {
        Some(m) => format!("attempt {attempt}/{m}"),
        None => format!("attempt {attempt}"),
    };
    format!(
        "[{name}] --- restarted ({reason}, {attempt_part}) ---",
        reason = reason.describe()
    )
}

/// Given the terminal status of the most recent child instance, decide
/// whether to restart and update the shared state accordingly. Returns
/// `true` if the caller should respawn, or `false` if the lifecycle task
/// should exit.
async fn apply_restart_decision(
    name_handle: &crate::model::NameHandle,
    state: &SharedState,
    final_status: ProcessStatus,
) -> bool {
    with_process_mut(state, name_handle, |runtime| {
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
    })
    .await
    .unwrap_or(false)
}

/// Main lifecycle loop for a running process: waits for exit, applies the
/// restart policy, backs off, and re-spawns. Runs as its own task; exits
/// when the process reaches a non-restarting terminal state.
async fn process_lifecycle(
    name_handle: crate::model::NameHandle,
    mut spec: crate::model::ProcessInstanceSpec,
    mut child: tokio::process::Child,
    mut kill_rx: watch::Receiver<bool>,
    state: SharedState,
) {
    loop {
        let (final_status, exit_reason) =
            wait_for_child_exit(&name_handle, &spec, &mut child, &mut kill_rx, &state).await;

        let should_restart = apply_restart_decision(&name_handle, &state, final_status).await;
        if !should_restart {
            let name = crate::model::read_name(&name_handle);
            let mut guard = state.lock().await;
            guard.controllers.remove(&name);
            break;
        }

        // Emit a separator line into the daemon log so that humans (and
        // `decompose logs svc`) can visually distinguish the previous run
        // from the next attempt. The separator flows through the same
        // stdout stream the child line-readers use, so name-prefix
        // filtering picks it up unchanged.
        if let Some(reason) = exit_reason {
            let (attempt, max) = with_process(&state, &name_handle, |r| {
                (r.restart_count, r.spec.max_restarts)
            })
            .await
            .unwrap_or((0, None));
            let name = crate::model::read_name(&name_handle);
            println!("{}", format_restart_separator(&name, reason, attempt, max));
        }

        // Backoff delay. Look up the current spec under the lock, since a
        // reload could have swapped it out while we were waiting.
        let backoff = with_process(&state, &name_handle, |r| r.spec.backoff_seconds)
            .await
            .unwrap_or(1);
        sleep(Duration::from_secs(backoff)).await;

        // Pick up any new spec the reload may have installed.
        let next_spec = with_process(&state, &name_handle, |r| r.spec.clone()).await;
        let Some(next_spec) = next_spec else { break };
        spec = next_spec;

        let Some(new_child) =
            spawn_process_child(&name_handle, &spec, &state, SpawnContext::Restart).await
        else {
            let name = crate::model::read_name(&name_handle);
            let mut guard = state.lock().await;
            guard.controllers.remove(&name);
            break;
        };
        child = new_child;
        let pid = child.id().unwrap_or(0);
        with_process_mut(&state, &name_handle, |runtime| {
            runtime.status = ProcessStatus::Running { pid };
            runtime.log_ready = false;
            runtime.ready = false;
            // A freshly spawned process is assumed alive until its
            // liveness probe (if any) proves otherwise — matches the
            // `ProcessRuntime::alive` default documented in model.rs.
            runtime.alive = true;
        })
        .await;

        let ready_pattern: Option<Regex> =
            spec.ready_log_line.as_deref().map(compile_ready_pattern);
        attach_output_readers(&mut child, &name_handle, ready_pattern, state.clone());

        // Fresh kill channel for this restart iteration, replacing the one
        // whose sender was dropped when `broadcast_stop` last fired.
        let (new_kill_tx, new_kill_rx) = watch::channel(false);
        {
            let name = crate::model::read_name(&name_handle);
            let mut guard = state.lock().await;
            guard.controllers.insert(name, new_kill_tx);
        }
        kill_rx = new_kill_rx;
    }
}

/// Spawn tasks that read lines from the child's stdout and stderr pipes,
/// printing them with a `[name]` prefix and optionally matching a
/// `ready_log_line` regex to set the `log_ready` flag on the process runtime.
fn attach_output_readers(
    child: &mut tokio::process::Child,
    name_handle: &crate::model::NameHandle,
    ready_pattern: Option<Regex>,
    state: SharedState,
) {
    let log_ready_flag = Arc::new(AtomicBool::new(false));

    if let Some(stdout) = child.stdout.take() {
        let handle = name_handle.clone();
        let pattern = ready_pattern.clone();
        let flag = log_ready_flag.clone();
        let state_clone = state.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let proc_name = crate::model::read_name(&handle);
                println!("[{proc_name}] {line}");
                if let Some(ref re) = pattern {
                    if !flag.load(Ordering::Relaxed) && re.is_match(&line) {
                        flag.store(true, Ordering::Relaxed);
                        with_process_mut(&state_clone, &handle, |runtime| {
                            runtime.log_ready = true;
                        })
                        .await;
                    }
                }
            }
        });
    }

    if let Some(stderr) = child.stderr.take() {
        let handle = name_handle.clone();
        let pattern = ready_pattern;
        let flag = log_ready_flag;
        let state_clone = state;
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let proc_name = crate::model::read_name(&handle);
                eprintln!("[{proc_name}] {line}");
                if let Some(ref re) = pattern {
                    if !flag.load(Ordering::Relaxed) && re.is_match(&line) {
                        flag.store(true, Ordering::Relaxed);
                        with_process_mut(&state_clone, &handle, |runtime| {
                            runtime.log_ready = true;
                        })
                        .await;
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
    let total_timeout =
        Duration::from_secs(timeout_override.unwrap_or(spec.shutdown_timeout_seconds));
    let deadline = tokio::time::Instant::now() + total_timeout;

    // Step 1: Run optional shutdown command. Bound it by the overall
    // shutdown timeout so a hung cleanup script can't block us forever.
    if let Some(ref cmd_str) = spec.shutdown_command {
        match build_shell_command(cmd_str) {
            Ok(mut cmd) => {
                cmd.current_dir(&spec.working_dir).envs(&spec.environment);
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    eprintln!(
                        "[{}] shutdown command {:?} skipped: no time budget remaining",
                        spec.name, cmd_str
                    );
                } else {
                    match tokio::time::timeout(remaining, cmd.output()).await {
                        Ok(Ok(output)) => {
                            if !output.status.success() {
                                let code = output
                                    .status
                                    .code()
                                    .map(|c| c.to_string())
                                    .unwrap_or_else(|| "signal".to_string());
                                eprintln!(
                                    "[{}] shutdown command {:?} exited with status {code}",
                                    spec.name, cmd_str
                                );
                            }
                        }
                        Ok(Err(e)) => {
                            eprintln!(
                                "[{}] shutdown command {:?} failed to spawn: {e}",
                                spec.name, cmd_str
                            );
                        }
                        Err(_) => {
                            eprintln!(
                                "[{}] shutdown command {:?} timed out; proceeding to SIGTERM",
                                spec.name, cmd_str
                            );
                        }
                    }
                }
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

    // Step 3: Wait for the remaining portion of the total timeout. If the
    // shutdown command ate most of it, we'll move on to SIGKILL quickly —
    // which is the right behaviour: the user's time budget is over.
    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
    let terminated = if remaining.is_zero() {
        false
    } else {
        tokio::time::timeout(remaining, child.wait()).await.is_ok()
    };

    if !terminated {
        // Step 4: Force kill
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
}

#[derive(Debug, Clone, Copy)]
enum ProbeKind {
    /// Flips the `ready` flag on success/failure thresholds. This is the
    /// signal `depends_on: process_healthy` gates on.
    Readiness,
    /// Flips the `alive` flag on success/failure thresholds, and on
    /// reaching `failure_threshold` SIGKILLs the process so the restart
    /// policy can re-launch it. Independent of `ready` — a service may
    /// configure both probes and each writes to its own flag.
    Liveness,
}

/// Spawn a probe task if a `HealthProbe` is configured. Combines the
/// previously-duplicated readiness/liveness setup blocks into one call.
fn spawn_probe_if_present(
    probe: Option<&HealthProbe>,
    kind: ProbeKind,
    name_handle: &crate::model::NameHandle,
    working_dir: &Path,
    environment: &BTreeMap<String, String>,
    state: &SharedState,
) {
    if let Some(probe) = probe {
        tokio::spawn(run_probe(
            kind,
            name_handle.clone(),
            probe.clone(),
            state.clone(),
            working_dir.to_path_buf(),
            environment.clone(),
        ));
    }
}

/// Run a health probe periodically. The polling/threshold scaffolding is
/// identical between readiness and liveness probes; only the success and
/// failure actions differ. See [`ProbeKind`] for the per-kind semantics.
async fn run_probe(
    kind: ProbeKind,
    name_handle: crate::model::NameHandle,
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
        let keep_going = with_process(&state, &name_handle, |r| !r.status.is_terminal())
            .await
            .unwrap_or(false);
        if !keep_going {
            break;
        }

        let success = run_single_check(&probe, &working_dir, &environment).await;

        if success {
            consecutive_successes += 1;
            consecutive_failures = 0;
            if consecutive_successes >= probe.success_threshold {
                with_process_mut(&state, &name_handle, |runtime| match kind {
                    ProbeKind::Readiness => runtime.ready = true,
                    ProbeKind::Liveness => runtime.alive = true,
                })
                .await;
            }
        } else {
            consecutive_failures += 1;
            consecutive_successes = 0;
            if consecutive_failures >= probe.failure_threshold {
                match kind {
                    ProbeKind::Readiness => {
                        with_process_mut(&state, &name_handle, |runtime| {
                            runtime.ready = false;
                        })
                        .await;
                    }
                    ProbeKind::Liveness => {
                        // Clear the liveness flag and kill the process so
                        // the restart policy can re-launch it. A fresh spawn
                        // resets `alive` back to `true` (see the Running
                        // transition in process_lifecycle / start_process).
                        with_process_mut(&state, &name_handle, |runtime| {
                            runtime.alive = false;
                            if let ProcessStatus::Running { pid } = runtime.status {
                                #[cfg(unix)]
                                {
                                    use nix::sys::signal::{self, Signal};
                                    use nix::unistd::Pid;
                                    let _ =
                                        signal::kill(Pid::from_raw(pid as i32), Signal::SIGKILL);
                                }
                                #[cfg(not(unix))]
                                {
                                    let _ = pid; // suppress unused warning
                                }
                            }
                        })
                        .await;

                        // Reset counter — the restart policy will re-launch
                        // the process and the probe will re-evaluate from
                        // scratch.
                        consecutive_failures = 0;

                        // Wait for the process to actually restart before
                        // probing again so we don't immediately kill the
                        // new instance.
                        sleep(Duration::from_secs(probe.period_seconds)).await;
                        continue;
                    }
                }
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

/// Resolve services and convert any unknown-name error into a
/// `Response::Error` so IPC handlers can exit early with `?`-style control
/// flow. Mirrors the check/format used by Stop/Start/Kill/Restart.
fn resolve_services_or_error(
    state: &DaemonState,
    services: &[String],
) -> std::result::Result<Vec<String>, Response> {
    resolve_services(state, services).map_err(|unknown| Response::Error {
        message: format!("unknown service(s): {}", unknown.join(", ")),
    })
}

/// Classification of a service across an old and new config snapshot.
///
/// A service's hash excludes `replicas` (see [`crate::config::compute_config_hash`]),
/// so a pure replica-count change lands in [`ReloadPlan::scaled`] — the daemon
/// adds or drops replica instances without disturbing the ones that remain.
/// Hash divergence always means `changed` (full recreate of every replica),
/// regardless of how the replica count moved.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ReloadPlan {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub changed: Vec<String>,
    pub unchanged: Vec<String>,
    /// Services whose hash is identical but whose replica count changed.
    /// Maps base-name → `(old_count, new_count)`. Handled by the daemon as
    /// spawn-up / stop-highest-index rather than a full recreate.
    pub scaled: BTreeMap<String, (u16, u16)>,
}

/// Summary of a single service's hash and replica count, keyed by the stable
/// base-name. Used as a simple input shape for [`compute_reload_plan`] so it
/// can be unit-tested without booting a full daemon.
#[derive(Debug, Clone)]
pub(crate) struct ServiceFingerprint {
    pub config_hash: String,
    pub replicas: u16,
}

/// Diff an old set of service fingerprints against a new one. `deps` maps
/// each *new* service to the base-names it depends on; if a removed service
/// is still referenced by a service that remains, returns an error describing
/// the violation — the caller must abort without touching any running
/// processes.
///
/// `force_recreate` promotes every service present in both snapshots to
/// `changed` regardless of hash equality. `no_recreate` demotes every
/// hash-diverged service to `unchanged` (the existing instances keep
/// running). Added and removed services are unaffected by either flag.
/// The caller is expected to enforce that the two flags are mutually
/// exclusive; this function asserts it in debug builds.
///
/// Classification rules for a service present in both snapshots:
///
/// | hash      | replicas  | flags                 | category   |
/// |-----------|-----------|-----------------------|------------|
/// | equal     | equal     | —                     | unchanged  |
/// | equal     | differ    | —                     | scaled     |
/// | equal     | differ    | `force_recreate=true` | changed    |
/// | equal     | differ    | `no_recreate=true`    | scaled     |
/// | differ    | any       | —                     | changed    |
/// | differ    | any       | `no_recreate=true`    | unchanged  |
/// | any       | any       | `force_recreate=true` | changed    |
///
/// `no_recreate` intentionally does NOT block a `scaled` transition:
/// adding or dropping replicas is not a recreate of existing instances,
/// so the flag's guarantee ("don't touch running processes") holds — we
/// only spawn or kill replicas at the tail of the replica set.
pub(crate) fn compute_reload_plan(
    old: &BTreeMap<String, ServiceFingerprint>,
    new: &BTreeMap<String, ServiceFingerprint>,
    deps: &BTreeMap<String, Vec<String>>,
    force_recreate: bool,
    no_recreate: bool,
) -> std::result::Result<ReloadPlan, String> {
    debug_assert!(
        !(force_recreate && no_recreate),
        "force_recreate and no_recreate are mutually exclusive"
    );

    let mut plan = ReloadPlan::default();

    for (name, new_fp) in new {
        match old.get(name) {
            None => plan.added.push(name.clone()),
            Some(old_fp) => {
                let hash_differs = old_fp.config_hash != new_fp.config_hash;
                let replicas_differ = old_fp.replicas != new_fp.replicas;

                if force_recreate {
                    // `--force-recreate` overrides everything: recreate all.
                    plan.changed.push(name.clone());
                } else if hash_differs {
                    // Real config change. `--no-recreate` demotes to unchanged;
                    // otherwise full recreate (which also covers any replica-
                    // count delta — the whole service is being rebuilt).
                    if no_recreate {
                        plan.unchanged.push(name.clone());
                    } else {
                        plan.changed.push(name.clone());
                    }
                } else if replicas_differ {
                    // Hash equal, replicas differ → scale. `--no-recreate`
                    // does not block this: scaling adds/drops replicas at the
                    // tail and leaves the others in place.
                    //
                    // Transitions across the `replicas == 1` boundary are
                    // still `scaled`: the daemon renames the single instance
                    // in place (`foo` ↔ `foo[1]`) so the existing process
                    // keeps its pid across a 1 → N or N → 1 reload. See
                    // `handle_reload` for the re-keying under the state lock.
                    plan.scaled
                        .insert(name.clone(), (old_fp.replicas, new_fp.replicas));
                } else {
                    plan.unchanged.push(name.clone());
                }
            }
        }
    }

    for name in old.keys() {
        if !new.contains_key(name) {
            plan.removed.push(name.clone());
        }
    }

    // Reject the request if a service that's being removed is still a
    // dependency of a service that remains (or is newly added). We check the
    // *new* dep graph — i.e. what the user's updated config asks for.
    for (svc, svc_deps) in deps {
        for dep in svc_deps {
            if plan.removed.contains(dep) {
                return Err(format!(
                    "cannot reload: service {svc:?} still depends on {dep:?}, which is removed in the new config"
                ));
            }
        }
    }

    plan.added.sort();
    plan.removed.sort();
    plan.changed.sort();
    plan.unchanged.sort();
    Ok(plan)
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

    // Refresh the orphan-watchdog activity clock on every request (not on
    // connection accept), so long-lived connections that drip-feed requests
    // also keep the daemon alive.
    {
        let mut guard = state.lock().await;
        guard.last_client_activity = Instant::now();
    }

    let response = match req {
        Request::Ping => {
            let guard = state.lock().await;
            Response::Pong {
                pid: std::process::id(),
                instance: guard.instance.clone(),
            }
        }
        Request::Ps => handle_ps(&state).await,
        Request::Down { timeout_seconds } => {
            let mut guard = state.lock().await;
            guard.shutdown_timeout_override = timeout_seconds;
            guard.request_shutdown();
            Response::Ack {
                message: "shutdown requested".to_string(),
            }
        }
        Request::Stop { services } => handle_stop(&state, services).await,
        Request::Start { services } => handle_start(&state, services).await,
        Request::Kill { services, signal } => handle_kill(&state, services, signal).await,
        Request::Restart { services } => handle_restart(&state, services).await,
        Request::RemoveOrphans { keep } => handle_remove_orphans(&state, keep).await,
        Request::Reload {
            force_recreate,
            no_recreate,
            remove_orphans,
            no_start,
        } => {
            handle_reload(
                state.clone(),
                force_recreate,
                no_recreate,
                remove_orphans,
                no_start,
            )
            .await
        }
        Request::ServiceRunState { name } => handle_service_run_state(&state, name).await,
    };

    let payload = serde_json::to_string(&response)?;
    write_half.write_all(payload.as_bytes()).await?;
    write_half.write_all(b"\n").await?;
    write_half.flush().await?;
    Ok(())
}

/// Snapshot every tracked process into a `Response::Ps`. Read-only; holds the
/// state lock just long enough to clone the runtime fields.
async fn handle_ps(state: &SharedState) -> Response {
    let guard = state.lock().await;
    let processes = guard
        .processes
        .values()
        .map(ProcessSnapshot::from)
        .collect::<Vec<_>>();

    Response::Ps {
        pid: std::process::id(),
        instance: guard.instance.clone(),
        processes,
    }
}

/// Stop the named services (or all services when `services` is empty).
/// Returns an error response if any name is unknown; otherwise triggers the
/// supervisor to begin shutdown and acks immediately.
async fn handle_stop(state: &SharedState, services: Vec<String>) -> Response {
    let mut guard = state.lock().await;
    match resolve_services_or_error(&guard, &services) {
        Err(resp) => resp,
        Ok(names) => {
            guard.stop_instances(&names);
            Response::Ack {
                message: format!("stopping {}", describe_services(&services)),
            }
        }
    }
}

/// Start the named services (or all services when `services` is empty). Walks
/// the dependency graph so transitively-depended-on services also get flipped
/// out of their terminal state, matching the behaviour `up` relies on.
async fn handle_start(state: &SharedState, services: Vec<String>) -> Response {
    let mut guard = state.lock().await;
    match resolve_services_or_error(&guard, &services) {
        Err(resp) => resp,
        Ok(names) => {
            // Collect the set of services to start, including their
            // transitive dependencies that are also in a terminal state
            // (e.g. NotStarted). This ensures `start serviceB` also
            // brings up serviceB's deps if they haven't been launched.
            let mut to_start: std::collections::BTreeSet<String> = names.into_iter().collect();
            let mut queue: std::collections::VecDeque<String> = to_start.iter().cloned().collect();
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
                        runtime.ready = false;
                        // `alive` defaults to true for a fresh
                        // instance — see ProcessRuntime docs.
                        runtime.alive = true;
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

/// Send `signal` to every Running instance of the named services. Unknown
/// services error out; services that aren't currently running are silently
/// skipped (the signal only has a target for actively-Running PIDs).
async fn handle_kill(state: &SharedState, services: Vec<String>, signal: i32) -> Response {
    let guard = state.lock().await;
    match resolve_services_or_error(&guard, &services) {
        Err(resp) => resp,
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

/// Restart the named services by stopping them and spawning a background task
/// that waits for each to reach a terminal state before flipping them back to
/// `Pending` so the supervisor respawns them.
async fn handle_restart(state: &SharedState, services: Vec<String>) -> Response {
    let mut guard = state.lock().await;
    match resolve_services_or_error(&guard, &services) {
        Err(resp) => resp,
        Ok(names) => {
            guard.stop_instances(&names);
            drop(guard);
            // Spawn a task to wait for stop then reset to Pending
            let state_clone = state.clone();
            let names_clone = names.clone();
            tokio::spawn(async move {
                wait_for_terminal(&state_clone, &names_clone, Duration::from_millis(50), 200).await;
                let mut guard = state_clone.lock().await;
                for name in &names_clone {
                    if let Some(runtime) = guard.processes.get_mut(name) {
                        runtime.status = ProcessStatus::Pending;
                        runtime.log_ready = false;
                        runtime.ready = false;
                        runtime.alive = true;
                    }
                }
            });
            Response::Ack {
                message: format!("restarting {}", describe_services(&services)),
            }
        }
    }
}

/// Stop and drop every process whose base-name is not in `keep`. Any orphan
/// instance is first sent a stop signal; a background task then waits for the
/// instances to go terminal before removing them from the process map.
async fn handle_remove_orphans(state: &SharedState, keep: Vec<String>) -> Response {
    let mut guard = state.lock().await;
    let keep_set: std::collections::HashSet<&str> = keep.iter().map(|s| s.as_str()).collect();
    let orphans: Vec<String> = guard
        .processes
        .keys()
        .filter(|name| {
            let base = guard.processes[name.as_str()].spec.base_name.as_str();
            !keep_set.contains(base)
        })
        .cloned()
        .collect();
    guard.stop_instances(&orphans);
    // Spawn a task to wait for orphans to stop then remove them
    if !orphans.is_empty() {
        let state_clone = state.clone();
        let orphans_clone = orphans.clone();
        tokio::spawn(async move {
            wait_for_terminal(&state_clone, &orphans_clone, Duration::from_millis(50), 200).await;
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

/// Report whether `name` matches any tracked service (by base-name or
/// fully-qualified replica name) and whether any matching replica is Running.
/// Used by `exec` to preflight-check before spawning.
async fn handle_service_run_state(state: &SharedState, name: String) -> Response {
    let guard = state.lock().await;
    let mut known = false;
    let mut any_running = false;
    for runtime in guard.processes.values() {
        if runtime.spec.base_name == name || runtime.spec.name == name {
            known = true;
            if matches!(runtime.status, ProcessStatus::Running { .. }) {
                any_running = true;
                break;
            }
        }
    }
    Response::ServiceRunState { known, any_running }
}

/// Re-read the daemon's config from disk, compute a reload plan against the
/// currently-tracked process map, and execute the plan.
///
/// On parse/validate failure: returns `Response::Error` and leaves running
/// processes untouched. On a removed-but-still-depended-on violation: same.
/// Otherwise: stops `changed` instances (they'll be respawned with the new
/// spec by the supervisor), optionally stops+removes `removed` services
/// when `remove_orphans` is true (otherwise they're left running with a
/// warning, matching Docker Compose default behaviour), and inserts
/// `added`/`changed` entries as `Pending` — or `NotStarted` when
/// `no_start` is true — so the supervisor loop launches them in
/// dependency order (or leaves them parked for a later `start`).
async fn handle_reload(
    state: SharedState,
    force_recreate: bool,
    no_recreate: bool,
    remove_orphans: bool,
    no_start: bool,
) -> Response {
    // Defence-in-depth: clap enforces this, but if a malformed IPC request
    // slips through with both flags set, refuse rather than silently picking
    // one interpretation.
    if force_recreate && no_recreate {
        return Response::Error {
            message: "reload: --force-recreate and --no-recreate are mutually exclusive"
                .to_string(),
        };
    }
    // Snapshot the daemon-launch args so we don't hold the state lock across
    // disk I/O.
    let (cwd, config_files, env_files, disable_dotenv) = {
        let guard = state.lock().await;
        (
            guard.cwd.clone(),
            guard.config_files.clone(),
            guard.env_files.clone(),
            guard.disable_dotenv,
        )
    };

    // 1. Load and interpolate the new config. Any failure here aborts the
    //    reload without touching any running processes.
    let dotenv = match load_dotenv_files(&cwd, &env_files, disable_dotenv) {
        Ok(d) => d,
        Err(e) => {
            return Response::Error {
                message: format!("reload: failed to load env files: {e:#}"),
            };
        }
    };

    let mut new_config = match load_and_merge_configs(&config_files) {
        Ok(c) => c,
        Err(e) => {
            return Response::Error {
                message: format!("reload: invalid config: {e:#}"),
            };
        }
    };
    apply_interpolation(&mut new_config);

    let new_process_map = build_process_instances(&new_config, &cwd, &dotenv);

    // 2. Build fingerprints keyed by base-name. Replicas share a config_hash,
    //    so collapsing to base-name is sufficient — and replica-count changes
    //    fall out naturally in the diff because we also include `replicas`
    //    (counted by iterating the instance map).
    let new_fingerprints: BTreeMap<String, ServiceFingerprint> = {
        let mut map: BTreeMap<String, (String, u16)> = BTreeMap::new();
        for runtime in new_process_map.values() {
            let entry = map
                .entry(runtime.spec.base_name.clone())
                .or_insert_with(|| (runtime.spec.config_hash.clone(), 0));
            entry.1 += 1;
        }
        map.into_iter()
            .map(|(k, (hash, replicas))| {
                (
                    k,
                    ServiceFingerprint {
                        config_hash: hash,
                        replicas,
                    },
                )
            })
            .collect()
    };

    let new_deps: BTreeMap<String, Vec<String>> = new_process_map
        .values()
        .map(|r| {
            (
                r.spec.base_name.clone(),
                r.spec.depends_on.keys().cloned().collect(),
            )
        })
        .collect();

    // 3. Snapshot current state under the lock.
    let old_fingerprints: BTreeMap<String, ServiceFingerprint> = {
        let guard = state.lock().await;
        let mut map: BTreeMap<String, (String, u16)> = BTreeMap::new();
        for runtime in guard.processes.values() {
            let entry = map
                .entry(runtime.spec.base_name.clone())
                .or_insert_with(|| (runtime.spec.config_hash.clone(), 0));
            entry.1 += 1;
        }
        map.into_iter()
            .map(|(k, (hash, replicas))| {
                (
                    k,
                    ServiceFingerprint {
                        config_hash: hash,
                        replicas,
                    },
                )
            })
            .collect()
    };

    // 4. Diff. A removed-but-still-depended-on violation returns early with
    //    an error; nothing has been touched yet.
    let plan = match compute_reload_plan(
        &old_fingerprints,
        &new_fingerprints,
        &new_deps,
        force_recreate,
        no_recreate,
    ) {
        Ok(p) => p,
        Err(msg) => return Response::Error { message: msg },
    };

    // 5. Execute the plan. We only touch running state past this point.

    // 5a. Collect the instance names whose lifecycle we're about to disrupt:
    //     the `changed` set always, plus the `removed` set when the caller
    //     asked us to clean orphans up. Both groups go through the same
    //     stop-then-wait pattern so we can splice in replacements (or drop
    //     the entries entirely, for removed orphans) without racing a
    //     still-shutting-down child.
    let changed_instances: Vec<String> = {
        let guard = state.lock().await;
        guard
            .processes
            .iter()
            .filter(|(_, r)| plan.changed.contains(&r.spec.base_name))
            .map(|(n, _)| n.clone())
            .collect()
    };

    let removed_instances: Vec<String> = if remove_orphans {
        let guard = state.lock().await;
        guard
            .processes
            .iter()
            .filter(|(_, r)| plan.removed.contains(&r.spec.base_name))
            .map(|(n, _)| n.clone())
            .collect()
    } else {
        Vec::new()
    };

    // Scale-down: for every `scaled` entry whose new count is strictly lower,
    // mark the tail replicas (indices `new+1..=old`) for stop+remove. The
    // lower-indexed replicas stay untouched.
    //
    // Special case `new_count == 1`: the surviving replica is `foo[1]`, which
    // must be renamed to the unqualified `foo` (see `rename_plan` below).
    // We still drop everything at index >= 2.
    let scaled_down_instances: Vec<String> = {
        let guard = state.lock().await;
        let mut to_drop = Vec::new();
        for (base, (old_count, new_count)) in &plan.scaled {
            if new_count < old_count {
                for (name, runtime) in &guard.processes {
                    if &runtime.spec.base_name == base && runtime.spec.replica > *new_count {
                        to_drop.push(name.clone());
                    }
                }
            }
        }
        to_drop
    };

    // Rename plan for scale transitions that cross the `replicas == 1` naming
    // boundary. See `build_process_instances`: a single-replica service uses
    // the unqualified base name (`foo`) while a multi-replica service uses
    // `foo[1]`, `foo[2]`, ….
    //
    //   1 → N: the surviving instance `foo`    → `foo[1]`; `foo[2..=N]` spawn
    //   N → 1: the surviving instance `foo[1]` → `foo`;    `foo[2..=N]` stop
    //
    // Tuples are `(old_name, new_name)`; applied atomically under the state
    // lock in the splice block below.
    let rename_plan: Vec<(String, String)> = plan
        .scaled
        .iter()
        .filter_map(|(base, (old_count, new_count))| {
            if *old_count == 1 && *new_count >= 2 {
                Some((base.clone(), format!("{base}[1]")))
            } else if *old_count >= 2 && *new_count == 1 {
                Some((format!("{base}[1]"), base.clone()))
            } else {
                None
            }
        })
        .collect();

    let to_stop: Vec<String> = changed_instances
        .iter()
        .chain(removed_instances.iter())
        .chain(scaled_down_instances.iter())
        .cloned()
        .collect();

    if !to_stop.is_empty() {
        let mut guard = state.lock().await;
        guard.stop_instances(&to_stop);
    }

    // 5b. Wait for the stopped instances to reach a terminal state so the
    //     replacement spawn isn't racing a still-shutting-down child. Poll
    //     briefly; the per-process shutdown timeout applies inside each
    //     controller's lifecycle task, so the outer wait bound just needs to
    //     exceed the worst-case shutdown.
    if !to_stop.is_empty() {
        wait_for_terminal(&state, &to_stop, Duration::from_millis(50), 1200).await;
    }

    // 5c. Under the lock: drop old changed + orphaned + scaled-down entries
    //     and their controllers, apply any rename transitions (1↔N boundary
    //     crossings preserve the existing pid in place), splice in new
    //     `added`/`changed` entries from `new_process_map`, and spawn any
    //     new replicas for scale-up transitions (leaving the existing
    //     replicas in place). With `no_start`, park every freshly inserted
    //     runtime in `NotStarted` so the supervisor leaves them alone;
    //     otherwise they land as `Pending` and the supervisor's next tick
    //     picks them up in dep order.
    let (n_added, n_changed, n_removed, n_scaled_services, replica_delta, n_renamed) = {
        let mut guard = state.lock().await;
        for name in &changed_instances {
            guard.processes.remove(name);
            guard.controllers.remove(name);
        }
        for name in &removed_instances {
            guard.processes.remove(name);
            guard.controllers.remove(name);
        }
        for name in &scaled_down_instances {
            guard.processes.remove(name);
            guard.controllers.remove(name);
        }

        // Apply in-place renames for 1↔N scale transitions. Both the map keys
        // and the shared `name_handle` inside the runtime are updated under
        // the same lock acquisition, so any daemon task that next reads the
        // handle sees the new name and looks up the correctly-keyed entry.
        let mut renamed = 0usize;
        for (old_name, new_name) in &rename_plan {
            let Some(mut runtime) = guard.processes.remove(old_name) else {
                // The surviving instance may have exited between the snapshot
                // and here; nothing to rename.
                continue;
            };
            // Update the `name` carried by the spec (visible through `ps`)
            // and the shared cell every task task consults before touching
            // `processes` / `controllers`.
            runtime.spec.name = new_name.clone();
            if let Ok(mut guard_name) = runtime.name_handle.write() {
                *guard_name = new_name.clone();
            }
            guard.processes.insert(new_name.clone(), runtime);
            if let Some(ctrl) = guard.controllers.remove(old_name) {
                guard.controllers.insert(new_name.clone(), ctrl);
            }
            renamed += 1;
        }

        let mut new_instances_added = 0usize;
        let mut new_instances_changed = 0usize;
        for (instance_name, runtime) in &new_process_map {
            let is_added = plan.added.contains(&runtime.spec.base_name);
            let is_changed = plan.changed.contains(&runtime.spec.base_name);
            // For `scaled` services, only insert replicas whose index exceeds
            // the old count — those are the newly-spawned tail entries.
            // Lower-indexed replicas already exist and must not be touched.
            //
            // For 1 → N transitions (`old_count == 1`), the `old_count` is
            // inclusive of the one existing instance (now renamed to
            // `foo[1]`), so the comparison correctly excludes replica 1 and
            // spawns only indices 2..=N. For N → 1 transitions the condition
            // `new_count > old_count` is false, so nothing is spliced in
            // here; the surviving instance is handled by `rename_plan`.
            let scaled_up_tail =
                plan.scaled
                    .get(&runtime.spec.base_name)
                    .is_some_and(|(old_count, new_count)| {
                        new_count > old_count && runtime.spec.replica > *old_count
                    });
            if !(is_added || is_changed || scaled_up_tail) {
                continue;
            }
            let mut runtime = runtime.clone();
            if no_start && matches!(runtime.status, ProcessStatus::Pending) {
                runtime.status = ProcessStatus::NotStarted;
            }
            guard.processes.insert(instance_name.clone(), runtime);
            if is_added {
                new_instances_added += 1;
            } else if is_changed {
                new_instances_changed += 1;
            }
            // scaled_up_tail contributes to `replica_delta` via plan.scaled,
            // so it doesn't need a separate counter here.
        }
        let removed_count = if remove_orphans {
            removed_instances.len()
        } else {
            plan.removed.len()
        };
        // Net replica delta across all `scaled` services, positive for
        // scale-up, negative for scale-down (reported as a signed count in
        // the ack so operators can see at a glance).
        let delta: i32 = plan
            .scaled
            .values()
            .map(|(old_count, new_count)| i32::from(*new_count) - i32::from(*old_count))
            .sum();
        (
            new_instances_added,
            new_instances_changed,
            removed_count,
            plan.scaled.len(),
            delta,
            renamed,
        )
    };

    if !plan.removed.is_empty() && !remove_orphans {
        eprintln!(
            "reload: orphan service(s) no longer in config (left running): [{}]",
            plan.removed.join(", ")
        );
    }

    let removed_label = if remove_orphans { "removed" } else { "orphan" };
    let delta_sign = if replica_delta >= 0 { "+" } else { "" };
    let rename_suffix = if n_renamed > 0 {
        format!(", {n_renamed} renamed")
    } else {
        String::new()
    };
    Response::Ack {
        message: format!(
            "reloaded: +{n_added} added, {n_changed} changed, \
             {n_scaled_services} scaled ({delta_sign}{replica_delta} replicas), \
             {n_removed} {removed_label}{rename_suffix}",
        ),
    }
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
                config_hash: String::new(),
            },
            status,
            started_once,
            log_ready: false,
            restart_count: 0,
            ready: false,
            alive: true,
            name_handle: crate::model::make_name_handle(base.to_string()),
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
    fn process_healthy_condition_uses_ready_flag_not_alive() {
        // Regression: when a service configures both readiness and liveness
        // probes, the flags must be independent. `process_healthy` gates on
        // readiness alone, so a process that is `alive` but not `ready`
        // must NOT satisfy the dependency.
        let mut snapshot = BTreeMap::new();
        let mut db = runtime_with("db", ProcessStatus::Running { pid: 42 }, true);
        db.ready = false;
        db.alive = true;
        snapshot.insert("db".to_string(), db);

        let mut candidate = runtime_with("api", ProcessStatus::Pending, false);
        candidate
            .spec
            .depends_on
            .insert("db".to_string(), DependencyCondition::ProcessHealthy);
        assert!(
            !dependencies_met(&candidate, &snapshot),
            "alive-but-not-ready must not satisfy process_healthy"
        );

        // Flip readiness on; liveness unchanged — should now satisfy.
        if let Some(db) = snapshot.get_mut("db") {
            db.ready = true;
        }
        assert!(dependencies_met(&candidate, &snapshot));

        // And the inverse: ready but not alive (liveness probe failing)
        // still satisfies `process_healthy` — liveness drives restarts, not
        // dependency gating.
        if let Some(db) = snapshot.get_mut("db") {
            db.alive = false;
        }
        assert!(
            dependencies_met(&candidate, &snapshot),
            "process_healthy must ignore alive; only readiness gates it"
        );
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

    fn fp(hash: &str, replicas: u16) -> ServiceFingerprint {
        ServiceFingerprint {
            config_hash: hash.to_string(),
            replicas,
        }
    }

    #[test]
    fn reload_plan_classifies_added_removed_changed_unchanged() {
        let mut old = BTreeMap::new();
        old.insert("api".to_string(), fp("h_api_v1", 1));
        old.insert("db".to_string(), fp("h_db", 1));
        old.insert("cache".to_string(), fp("h_cache", 1));

        let mut new = BTreeMap::new();
        // changed: hash differs
        new.insert("api".to_string(), fp("h_api_v2", 1));
        // unchanged
        new.insert("db".to_string(), fp("h_db", 1));
        // added
        new.insert("worker".to_string(), fp("h_worker", 1));
        // "cache" absent from new -> removed

        let deps: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let plan = compute_reload_plan(&old, &new, &deps, false, false).expect("no dep violations");

        assert_eq!(plan.added, vec!["worker".to_string()]);
        assert_eq!(plan.removed, vec!["cache".to_string()]);
        assert_eq!(plan.changed, vec!["api".to_string()]);
        assert_eq!(plan.unchanged, vec!["db".to_string()]);
    }

    #[test]
    fn reload_plan_scales_pure_replica_count_change() {
        // 2 → 3: no naming boundary crossing, hash equal → scaled.
        let mut old = BTreeMap::new();
        old.insert("worker".to_string(), fp("same_hash", 2));

        let mut new = BTreeMap::new();
        new.insert("worker".to_string(), fp("same_hash", 3));

        let plan = compute_reload_plan(&old, &new, &BTreeMap::new(), false, false)
            .expect("no dep violations");
        assert!(
            plan.changed.is_empty(),
            "pure replica change should not recreate: {plan:?}"
        );
        assert!(
            plan.unchanged.is_empty(),
            "pure replica change should not be unchanged: {plan:?}"
        );
        assert_eq!(plan.scaled.get("worker"), Some(&(2, 3)));
    }

    #[test]
    fn reload_plan_scale_down_classifies_as_scaled() {
        // 5 → 2: no naming boundary crossing.
        let mut old = BTreeMap::new();
        old.insert("worker".to_string(), fp("same_hash", 5));

        let mut new = BTreeMap::new();
        new.insert("worker".to_string(), fp("same_hash", 2));

        let plan = compute_reload_plan(&old, &new, &BTreeMap::new(), false, false)
            .expect("no dep violations");
        assert!(plan.changed.is_empty());
        assert_eq!(plan.scaled.get("worker"), Some(&(5, 2)));
    }

    #[test]
    fn reload_plan_replica_change_plus_hash_change_is_changed() {
        // Hash divergence dominates: full recreate regardless of replica delta.
        let mut old = BTreeMap::new();
        old.insert("worker".to_string(), fp("h_v1", 2));

        let mut new = BTreeMap::new();
        new.insert("worker".to_string(), fp("h_v2", 3));

        let plan = compute_reload_plan(&old, &new, &BTreeMap::new(), false, false)
            .expect("no dep violations");
        assert_eq!(plan.changed, vec!["worker".to_string()]);
        assert!(plan.scaled.is_empty());
    }

    #[test]
    fn reload_plan_force_recreate_overrides_scale() {
        let mut old = BTreeMap::new();
        old.insert("worker".to_string(), fp("same_hash", 2));

        let mut new = BTreeMap::new();
        new.insert("worker".to_string(), fp("same_hash", 3));

        let plan = compute_reload_plan(&old, &new, &BTreeMap::new(), true, false)
            .expect("force_recreate valid");
        assert_eq!(plan.changed, vec!["worker".to_string()]);
        assert!(plan.scaled.is_empty());
    }

    #[test]
    fn reload_plan_no_recreate_does_not_block_scale() {
        // `--no-recreate` is about not recreating existing instances. Scaling
        // only adds/drops tail replicas, so it should still apply.
        let mut old = BTreeMap::new();
        old.insert("worker".to_string(), fp("same_hash", 2));

        let mut new = BTreeMap::new();
        new.insert("worker".to_string(), fp("same_hash", 3));

        let plan = compute_reload_plan(&old, &new, &BTreeMap::new(), false, true)
            .expect("no_recreate valid");
        assert!(plan.changed.is_empty());
        assert_eq!(plan.scaled.get("worker"), Some(&(2, 3)));
    }

    #[test]
    fn reload_plan_replica_1_boundary_is_scaled_with_rename() {
        // 1 ↔ N transitions cross the replica-name boundary (`foo` vs `foo[1]`)
        // but are still classified as `scaled` — the daemon renames the
        // surviving instance in place rather than recreating it.
        for (old_count, new_count) in [(1u16, 2u16), (2, 1), (1, 3), (3, 1)] {
            let mut old = BTreeMap::new();
            old.insert("worker".to_string(), fp("same_hash", old_count));
            let mut new = BTreeMap::new();
            new.insert("worker".to_string(), fp("same_hash", new_count));

            let plan = compute_reload_plan(&old, &new, &BTreeMap::new(), false, false)
                .expect("valid transition");
            assert!(
                plan.changed.is_empty(),
                "{old_count}→{new_count} should not be a full recreate"
            );
            assert_eq!(
                plan.scaled.get("worker"),
                Some(&(old_count, new_count)),
                "{old_count}→{new_count} should be scaled"
            );
        }
    }

    #[test]
    fn reload_plan_rejects_removed_service_still_depended_on() {
        let mut old = BTreeMap::new();
        old.insert("api".to_string(), fp("h_api", 1));
        old.insert("db".to_string(), fp("h_db", 1));

        // "db" is gone in the new config, but "api" (which remains) still
        // lists it as a dep.
        let mut new = BTreeMap::new();
        new.insert("api".to_string(), fp("h_api", 1));

        let mut deps: BTreeMap<String, Vec<String>> = BTreeMap::new();
        deps.insert("api".to_string(), vec!["db".to_string()]);

        let err = compute_reload_plan(&old, &new, &deps, false, false)
            .expect_err("must reject removed-but-still-depended-on");
        assert!(err.contains("api"), "error mentions dependent: {err}");
        assert!(err.contains("db"), "error mentions removed dep: {err}");
    }

    #[test]
    fn reload_plan_allows_removal_when_no_remaining_service_depends_on_it() {
        let mut old = BTreeMap::new();
        old.insert("api".to_string(), fp("h_api", 1));
        old.insert("legacy".to_string(), fp("h_legacy", 1));

        let mut new = BTreeMap::new();
        new.insert("api".to_string(), fp("h_api", 1));

        // api does not depend on legacy — removing legacy is fine.
        let deps: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let plan =
            compute_reload_plan(&old, &new, &deps, false, false).expect("removal is allowed");
        assert_eq!(plan.removed, vec!["legacy".to_string()]);
    }

    #[test]
    fn reload_plan_force_recreate_promotes_unchanged_to_changed() {
        let mut old = BTreeMap::new();
        old.insert("api".to_string(), fp("same_hash", 1));
        old.insert("db".to_string(), fp("h_db", 1));

        let mut new = BTreeMap::new();
        // hash-equal in both — without force_recreate this would be unchanged.
        new.insert("api".to_string(), fp("same_hash", 1));
        new.insert("db".to_string(), fp("h_db", 1));

        let deps: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let plan = compute_reload_plan(&old, &new, &deps, true, false)
            .expect("force_recreate is a valid option");

        assert_eq!(
            plan.changed,
            vec!["api".to_string(), "db".to_string()],
            "force_recreate promotes every still-present service to changed"
        );
        assert!(
            plan.unchanged.is_empty(),
            "force_recreate should leave nothing in unchanged"
        );
    }

    #[test]
    fn reload_plan_no_recreate_demotes_hash_diverged_to_unchanged() {
        let mut old = BTreeMap::new();
        old.insert("api".to_string(), fp("h_api_v1", 1));
        old.insert("db".to_string(), fp("h_db", 1));

        let mut new = BTreeMap::new();
        // api's hash differs — normally changed, but no_recreate keeps it.
        new.insert("api".to_string(), fp("h_api_v2", 1));
        new.insert("db".to_string(), fp("h_db", 1));
        // newly-added service should still be added regardless of no_recreate.
        new.insert("worker".to_string(), fp("h_worker", 1));

        let deps: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let plan = compute_reload_plan(&old, &new, &deps, false, true)
            .expect("no_recreate is a valid option");

        assert!(
            plan.changed.is_empty(),
            "no_recreate should keep every still-present service as unchanged"
        );
        assert_eq!(
            plan.unchanged,
            vec!["api".to_string(), "db".to_string()],
            "hash-diverged api was demoted to unchanged"
        );
        assert_eq!(
            plan.added,
            vec!["worker".to_string()],
            "added services aren't affected by no_recreate"
        );
    }

    #[test]
    fn reload_plan_default_flags_preserve_existing_behavior() {
        let mut old = BTreeMap::new();
        old.insert("api".to_string(), fp("h_api_v1", 1));
        old.insert("db".to_string(), fp("h_db", 1));

        let mut new = BTreeMap::new();
        new.insert("api".to_string(), fp("h_api_v2", 1));
        new.insert("db".to_string(), fp("h_db", 1));

        let deps: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let plan =
            compute_reload_plan(&old, &new, &deps, false, false).expect("default flags are valid");

        assert_eq!(plan.changed, vec!["api".to_string()]);
        assert_eq!(plan.unchanged, vec!["db".to_string()]);
    }

    #[test]
    fn restart_separator_formats_exit_code_with_max() {
        let line = format_restart_separator("api", ExitReason::Code(1), 2, Some(5));
        assert_eq!(line, "[api] --- restarted (exit code 1, attempt 2/5) ---");
    }

    #[test]
    fn restart_separator_formats_exit_code_without_max() {
        let line = format_restart_separator("api", ExitReason::Code(1), 2, None);
        assert_eq!(line, "[api] --- restarted (exit code 1, attempt 2) ---");
    }

    #[test]
    fn restart_separator_formats_signal_with_known_name() {
        // SIGTERM is 15 on every Unix platform we care about.
        let line = format_restart_separator("api", ExitReason::Signal(15), 1, None);
        assert_eq!(line, "[api] --- restarted (signal SIGTERM, attempt 1) ---");
    }

    #[test]
    fn restart_separator_formats_unknown_signal() {
        // A signal number nix doesn't know about falls back to the number.
        let line = format_restart_separator("worker", ExitReason::Signal(999), 3, Some(10));
        assert_eq!(
            line,
            "[worker] --- restarted (signal 999, attempt 3/10) ---"
        );
    }

    #[test]
    fn restart_separator_preserves_process_name_prefix_for_filtering() {
        // The `[name]` prefix is what `decompose logs NAME` uses to filter,
        // so it must exactly match the stdout/stderr reader format.
        let line = format_restart_separator("my-service", ExitReason::Code(0), 1, None);
        assert!(
            line.starts_with("[my-service] "),
            "expected name-prefixed line, got {line:?}"
        );
    }
}
