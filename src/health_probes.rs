//! Health probe runtime.
//!
//! The daemon exposes two user-configurable probes per process — a readiness
//! probe and a liveness probe — and both follow the same scheduling pattern:
//!   1. wait for `initial_delay_seconds`
//!   2. every `period_seconds`, run one check with `timeout_seconds`
//!   3. track `consecutive_successes` / `consecutive_failures`
//!   4. on reaching `success_threshold`, flag the process as
//!      ready/alive; on `failure_threshold`, clear the flag (and, for
//!      liveness, SIGKILL so the restart policy re-launches it).
//!
//! This module owns everything from step 1 onward: the periodic loop, the
//! dispatch over `exec` / `http_get` check kinds, and the threshold state
//! machine. The supervisor in `daemon.rs` decides *when* to start a probe
//! (on process spawn) and observes the flag changes via its own state
//! inspection (dependency gating, restart decisions).
//!
//! Semantics must stay bit-identical to the previous inline implementation;
//! this is a pure refactor.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::time::sleep;

use crate::daemon::{SharedState, build_shell_command, with_process, with_process_mut};
use crate::model::{HealthProbe, HttpCheck, NameHandle, ProcessStatus};

/// Which flag the probe writes to on threshold crossings.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ProbeKind {
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
pub(crate) fn spawn_probe_if_present(
    probe: Option<&HealthProbe>,
    kind: ProbeKind,
    name_handle: &NameHandle,
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
    name_handle: NameHandle,
    probe: HealthProbe,
    state: SharedState,
    working_dir: PathBuf,
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

async fn http_get_check(http: &HttpCheck) -> bool {
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
