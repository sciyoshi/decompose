//! Unix process-group ownership and bounded shutdown.
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::process::Child;
use tokio::time::{Instant, sleep};

use crate::model::ProcessInstanceSpec;

#[derive(Serialize, Deserialize)]
pub(crate) struct Receipt {
    pub pid: u32,
    pub errors: Vec<String>,
}

pub(crate) struct Signals {
    #[cfg(unix)]
    int: tokio::signal::unix::Signal,
    #[cfg(unix)]
    term: tokio::signal::unix::Signal,
}

impl Signals {
    pub fn new() -> Result<Self> {
        Ok(Self {
            #[cfg(unix)]
            int: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?,
            #[cfg(unix)]
            term: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?,
        })
    }

    pub async fn recv(&mut self) {
        #[cfg(unix)]
        tokio::select! { _ = self.int.recv() => {}, _ = self.term.recv() => {} }
        #[cfg(not(unix))]
        let _ = tokio::signal::ctrl_c().await;
    }
}

pub(crate) fn signal_group(pgid: u32, signal: i32) -> Result<()> {
    #[cfg(unix)]
    {
        use nix::{
            errno::Errno,
            sys::signal::{Signal, kill},
            unistd::Pid,
        };
        let signal = Signal::try_from(signal).context("invalid shutdown signal")?;
        match kill(Pid::from_raw(-(pgid as i32)), signal) {
            Ok(()) | Err(Errno::ESRCH) => Ok(()),
            // macOS reports EPERM for zombie-only groups. A member can
            // exit between inspection and kill, so verify the group again
            // before treating this as a real permission failure.
            #[cfg(target_os = "macos")]
            Err(Errno::EPERM) if !crate::process_table::group_alive(pgid)? => Ok(()),
            Err(e) => Err(e).context("failed to signal process group"),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (pgid, signal);
        Ok(())
    }
}

/// Inspect native process state off the async executor. Zombies are finished
/// even while their new parent has yet to reap them.
async fn group_alive(pgid: u32) -> Result<bool> {
    #[cfg(unix)]
    {
        Ok(tokio::task::spawn_blocking(move || crate::process_table::group_alive(pgid)).await??)
    }
    #[cfg(not(unix))]
    {
        let _ = pgid;
        Ok(false)
    }
}

async fn finished(child: &mut Child, pgid: u32) -> Result<bool> {
    let exited = child.try_wait()?.is_some();
    Ok(exited && !group_alive(pgid).await?)
}

/// Also protects short-lived helper commands when their task is cancelled.
struct GroupGuard(u32);
impl Drop for GroupGuard {
    fn drop(&mut self) {
        let _ = signal_group(self.0, 9);
    }
}

pub(crate) async fn run_hook(
    command: &str,
    spec: &ProcessInstanceSpec,
    deadline: Instant,
    force: &AtomicBool,
) -> Result<()> {
    if Instant::now() >= deadline || force.load(Ordering::Relaxed) {
        return Ok(());
    }
    let mut cmd = crate::daemon::build_shell_command(command)?;
    cmd.current_dir(&spec.working_dir)
        .envs(&spec.environment)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);
    let mut child = cmd.spawn().context("spawn shutdown hook")?;
    let pgid = child.id().expect("live hook has pid");
    let guard = GroupGuard(pgid);
    loop {
        if finished(&mut child, pgid).await? {
            break;
        }
        if Instant::now() >= deadline || force.load(Ordering::Relaxed) {
            signal_group(pgid, 9)?;
            #[cfg(not(unix))]
            child.start_kill()?;
            drain(&mut child, pgid).await?;
            eprintln!("[{}] shutdown hook timed out or was interrupted", spec.name);
            break;
        }
        sleep(Duration::from_millis(50)).await;
    }
    // No processes remain, so do not signal a subsequently recycled pgid.
    std::mem::forget(guard);
    let status = child.wait().await?;
    if !status.success() {
        eprintln!("[{}] shutdown hook exited with {status}", spec.name);
    }
    Ok(())
}

async fn drain(child: &mut Child, pgid: u32) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if finished(child, pgid).await? {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("process group {pgid} survived SIGKILL");
        }
        sleep(Duration::from_millis(50)).await;
    }
}

pub(crate) async fn cleanup(
    child: &mut Child,
    pgid: u32,
    spec: &ProcessInstanceSpec,
    timeout: Duration,
    explicit: bool,
    force: &AtomicBool,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    // A normally exited leader can still have live descendants. They belong
    // to this generation and must finish before a replacement is launched.
    if !explicit && finished(child, pgid).await? {
        return Ok(());
    }
    let hook_error = if explicit && let Some(command) = &spec.shutdown_command {
        run_hook(command, spec, deadline, force).await.err()
    } else {
        None
    };
    let signal_error = signal_group(pgid, spec.shutdown_signal.unwrap_or(15)).err();
    #[cfg(not(unix))]
    child.start_kill()?;
    loop {
        if finished(child, pgid).await? {
            break;
        }
        if force.load(Ordering::Relaxed) || Instant::now() >= deadline || signal_error.is_some() {
            signal_group(pgid, 9)?;
            #[cfg(not(unix))]
            child.start_kill()?;
            drain(child, pgid).await?;
            break;
        }
        sleep(Duration::from_millis(50)).await;
    }
    if let Some(error) = hook_error.or(signal_error) {
        return Err(error);
    }
    Ok(())
}

/// Run an exec probe in its own group and always reap it before returning.
pub(crate) async fn probe_command(
    mut cmd: tokio::process::Command,
    timeout: Duration,
    cancel: &mut tokio::sync::watch::Receiver<bool>,
) -> Result<bool> {
    if *cancel.borrow() {
        return Ok(false);
    }
    cmd.kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(_) => return Ok(false),
    };
    let pgid = child.id().expect("live probe has pid");
    let guard = GroupGuard(pgid);
    let status = tokio::select! {
        biased;
        _ = cancel.changed() => None,
        result = tokio::time::timeout(timeout, child.wait()) => result.ok().transpose()?,
    };
    signal_group(pgid, 9)?;
    #[cfg(not(unix))]
    child.start_kill()?;
    drain(&mut child, pgid).await?;
    std::mem::forget(guard);
    Ok(status.is_some_and(|s| s.success()))
}
