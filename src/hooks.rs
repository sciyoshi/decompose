//! Bounded, cancellable startup steps owned by a replica controller.
use std::process::Stdio;
use std::time::{Duration, SystemTime};

use tokio::io::BufReader;
use tokio::sync::watch;
use tokio::time::{Instant, sleep, sleep_until};

use crate::config::HookConfig;
use crate::daemon::{SharedState, with_process_mut};
use crate::model::{
    HookRecord, Initialization, InitializationState, NameHandle, ProcessInstanceSpec, ProcessStatus,
};

fn now() -> String {
    humantime::format_rfc3339_nanos(SystemTime::now()).to_string()
}

pub(crate) async fn begin(state: &SharedState, handle: &NameHandle, spec: &ProcessInstanceSpec) {
    with_process_mut(state, handle, |r| {
        r.hook_cancel = None;
        r.status = ProcessStatus::Initializing;
        r.initialization = Initialization {
            state: InitializationState::Running,
            hooks: [
                ("pre_start", &spec.pre_start),
                ("post_start", &spec.post_start),
            ]
            .into_iter()
            .flat_map(|(phase, hooks)| {
                hooks.iter().map(move |h| HookRecord {
                    phase: phase.into(),
                    name: h.name.clone(),
                    stage: None,
                    status: "pending".into(),
                    started_at: None,
                    finished_at: None,
                    exit_code: None,
                    error: None,
                    reason: None,
                })
            })
            .collect(),
            ..Default::default()
        };
    })
    .await;
}

pub(crate) async fn end_incarnation(state: &SharedState, handle: &NameHandle, exited: bool) {
    with_process_mut(state, handle, |r| {
        r.hook_cancel = None;
        r.initialization.initialized = false;
        if r.initialization.state == InitializationState::Running {
            r.initialization.state = InitializationState::Cancelled;
        }
        for h in &mut r.initialization.hooks {
            if h.status == "pending"
                || (exited && h.status == "skipped" && h.reason.as_deref() == Some("cancelled"))
            {
                h.status = "skipped".into();
                h.reason = Some(
                    if exited {
                        "service_exited"
                    } else {
                        "cancelled"
                    }
                    .into(),
                );
                h.finished_at = Some(now());
            }
        }
    })
    .await;
}

#[derive(Debug)]
struct Failure {
    message: String,
    code: Option<i32>,
    cancelled: bool,
}
impl Failure {
    fn error(message: impl ToString) -> Self {
        Self {
            message: message.to_string(),
            code: None,
            cancelled: false,
        }
    }
    fn cancelled() -> Self {
        Self {
            message: "cancelled".into(),
            code: None,
            cancelled: true,
        }
    }
    fn exit(code: Option<i32>, stage: &str) -> Self {
        Self {
            message: format!(
                "{stage} exited with {}",
                code.map(|c| c.to_string())
                    .unwrap_or_else(|| "signal".into())
            ),
            code,
            cancelled: false,
        }
    }
}
type Result<T> = std::result::Result<T, Failure>;

struct Step<'a> {
    state: &'a SharedState,
    handle: &'a NameHandle,
    spec: &'a ProcessInstanceSpec,
    hook: &'a HookConfig,
    phase: &'a str,
    deadline: Instant,
    cancel: watch::Receiver<bool>,
}
impl Step<'_> {
    fn check(&self) -> Result<()> {
        if *self.cancel.borrow() {
            return Err(Failure::cancelled());
        }
        if Instant::now() >= self.deadline {
            return Err(Failure::error("overall hook deadline exceeded"));
        }
        Ok(())
    }
    async fn stage(&self, stage: &str) -> Result<()> {
        self.check()?;
        with_process_mut(self.state, self.handle, |r| {
            let i = &mut r.initialization;
            i.phase = Some(self.phase.into());
            i.hook = Some(self.hook.name.clone());
            if let Some(h) = i
                .hooks
                .iter_mut()
                .find(|h| h.phase == self.phase && h.name == self.hook.name)
            {
                h.stage = Some(stage.into());
            }
        })
        .await;
        self.check()
    }
    async fn log(&self, stage: &str, message: &str) {
        let logs = self.state.lock().await.logs.clone();
        if let Ok(writer) = logs.writer(&self.spec.base_name, self.spec.replica) {
            let name = crate::model::read_name(self.handle);
            let metadata = (
                self.phase.to_owned(),
                self.hook.name.clone(),
                stage.to_owned(),
            );
            let message = message.to_owned();
            let _ = tokio::task::spawn_blocking(move || {
                writer.lock().unwrap().write_hook(
                    &name,
                    "event",
                    &message,
                    false,
                    (&metadata.0, &metadata.1, &metadata.2),
                )
            })
            .await;
        }
    }
    async fn pause(&mut self, seconds: u64) -> Result<()> {
        self.check()?;
        tokio::select! {
            biased;
            _ = self.cancel.changed() => Err(Failure::cancelled()),
            _ = sleep_until(self.deadline) => Err(Failure::error("overall hook deadline exceeded")),
            _ = sleep(Duration::from_secs(seconds)) => Ok(()),
        }
    }
    async fn artifact(&self) -> Result<bool> {
        self.check()?;
        let Some(path) = &self.hook.creates else {
            return Ok(false);
        };
        match tokio::fs::metadata(path).await {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(Failure::error(format!("artifact {}: {e}", path.display()))),
        }
    }
    // Every subprocess owns a group. Timeout/cancellation always joins cleanup
    // and output readers before returning, including when the shell exited first.
    async fn command(
        &mut self,
        command: &str,
        stage: &str,
        attempt_deadline: Instant,
    ) -> Result<Option<i32>> {
        self.stage(stage).await?;
        let mut cmd = crate::daemon::build_shell_command(command).map_err(Failure::error)?;
        cmd.current_dir(
            self.hook
                .working_dir
                .as_ref()
                .expect("resolved hook directory"),
        )
        .envs(&self.hook.environment.0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
        #[cfg(unix)]
        cmd.process_group(0);
        let mut child = cmd
            .spawn()
            .map_err(|e| Failure::error(format!("cannot spawn {stage}: {e}")))?;
        let pgid = child.id().expect("live hook PID");
        let logs = self.state.lock().await.logs.clone();
        let writer = logs.writer(&self.spec.base_name, self.spec.replica).ok();
        let streams: Vec<(&str, std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>>)> = vec![
            ("stdout", Box::pin(child.stdout.take().unwrap())),
            ("stderr", Box::pin(child.stderr.take().unwrap())),
        ];
        let mut readers = Vec::new();
        for (stream, pipe) in streams {
            let writer = writer.clone();
            let handle = self.handle.clone();
            let phase = self.phase.to_owned();
            let name = self.hook.name.clone();
            let stage = stage.to_owned();
            readers.push(tokio::spawn(async move {
                let mut chunks = crate::logs::Chunks::new(BufReader::new(pipe));
                while let Ok(Some((line, partial))) = chunks.next().await {
                    if let Some(writer) = writer.clone() {
                        let current = crate::model::read_name(&handle);
                        let metadata = (phase.clone(), name.clone(), stage.clone());
                        let _ = tokio::task::spawn_blocking(move || {
                            writer.lock().unwrap().write_hook(
                                &current,
                                stream,
                                &line,
                                partial,
                                (&metadata.0, &metadata.1, &metadata.2),
                            )
                        })
                        .await;
                    }
                }
            }));
        }
        let result = tokio::select! {
            biased;
            _ = async { if !*self.cancel.borrow() { let _ = self.cancel.changed().await; } } => Err(Failure::cancelled()),
            _ = sleep_until(attempt_deadline.min(self.deadline)) => Err(Failure::error("command deadline exceeded")),
            status = child.wait() => status.map(|s| s.code()).map_err(Failure::error),
        };
        let mut cleanup_spec = self.spec.clone();
        cleanup_spec.shutdown_command = None;
        cleanup_spec.shutdown_signal = Some(15);
        let force = self.state.lock().await.force_shutdown.clone();
        let cleanup = crate::shutdown::cleanup(
            &mut child,
            pgid,
            &cleanup_spec,
            Duration::from_secs(5),
            false,
            &force,
        )
        .await;
        if let Err(e) = cleanup {
            // Block replacement if descendants cannot be proven dead.
            crate::daemon::record_hook_cleanup_error(
                self.state,
                self.handle,
                format!("hook cleanup failed: {e:#}"),
            )
            .await;
            for reader in readers {
                reader.abort();
            }
            return Err(Failure::error(e));
        }
        for reader in readers {
            let _ = reader.await;
        }
        result
    }
    async fn guard(&mut self, verify: bool) -> Result<bool> {
        let stage = if verify { "verifying" } else { "checking" };
        self.stage(stage).await?;
        if let Some(command) = &self.hook.unless {
            let command = command.clone();
            match self.command(&command, stage, self.deadline).await? {
                Some(0) => Ok(true),
                Some(1) if !verify => Ok(false),
                code => Err(Failure::exit(code, stage)),
            }
        } else {
            self.artifact().await
        }
    }
    async fn run(&mut self) -> Result<bool> {
        self.check()?;
        // The working directory is required even for an already-satisfied guard.
        let dir = self.hook.working_dir.as_ref().unwrap();
        if !tokio::fs::metadata(dir)
            .await
            .map_err(Failure::error)?
            .is_dir()
        {
            return Err(Failure::error("hook working_dir is not a directory"));
        }
        if self.hook.creates.is_some() && self.artifact().await? {
            return Ok(true);
        }
        if let Some(wait) = self.hook.wait_for.clone() {
            self.stage("waiting").await?;
            self.pause(wait.initial_delay_seconds).await?;
            loop {
                self.check()?;
                let deadline =
                    (Instant::now() + Duration::from_secs(wait.timeout_seconds)).min(self.deadline);
                let passed = if let Some(exec) = &wait.exec {
                    match self.command(&exec.command, "waiting", deadline).await {
                        Ok(Some(0)) => true,
                        Ok(_) => false,
                        Err(e) if !e.cancelled && e.message == "command deadline exceeded" => false,
                        Err(e) => return Err(e),
                    }
                } else {
                    let http = wait.http_get.as_ref().unwrap();
                    let http = crate::model::HttpCheck {
                        host: http.host.clone(),
                        port: http.port,
                        scheme: http.scheme.clone(),
                        path: http.path.clone(),
                    };
                    tokio::select! {
                        biased;
                        _ = self.cancel.changed() => return Err(Failure::cancelled()),
                        value = tokio::time::timeout_at(deadline, crate::health_probes::http_get_check(&http)) => value.unwrap_or(false),
                    }
                };
                if passed {
                    break;
                }
                self.pause(wait.period_seconds).await?;
            }
        }
        if self.guard(false).await? {
            return Ok(true);
        }
        let command = self.hook.command.clone();
        let code = self.command(&command, "executing", self.deadline).await?;
        if code != Some(0) {
            return Err(Failure::exit(code, "executing"));
        }
        if (self.hook.unless.is_some() || self.hook.creates.is_some()) && !self.guard(true).await? {
            return Err(Failure::error(
                "verification failed: artifact was not created",
            ));
        }
        self.check()?;
        Ok(false)
    }
}

/// Returns true only if every step in this phase succeeded or was satisfied.
pub(crate) async fn run_phase(
    state: &SharedState,
    handle: &NameHandle,
    spec: &ProcessInstanceSpec,
    phase: &str,
    cancel: watch::Receiver<bool>,
) -> bool {
    let hooks = if phase == "pre_start" {
        &spec.pre_start
    } else {
        &spec.post_start
    };
    for hook in hooks {
        let mut step = Step {
            state,
            handle,
            spec,
            hook,
            phase,
            cancel: cancel.clone(),
            deadline: Instant::now() + Duration::from_secs(hook.timeout_seconds),
        };
        with_process_mut(state, handle, |r| {
            r.initialization.phase = Some(phase.into());
            r.initialization.hook = Some(hook.name.clone());
            let h = r
                .initialization
                .hooks
                .iter_mut()
                .find(|h| h.phase == phase && h.name == hook.name)
                .unwrap();
            h.status = "running".into();
            h.started_at = Some(now());
        })
        .await;
        step.log("checking", "hook started").await;
        let result = step.run().await;
        let message = match &result {
            Ok(true) => "hook skipped: already_satisfied".into(),
            Ok(false) => "hook succeeded".into(),
            Err(e) if e.cancelled => "hook cancelled".into(),
            Err(e) => format!("hook failed: {}", e.message),
        };
        with_process_mut(state, handle, |r| {
            let i = &mut r.initialization;
            let h = i
                .hooks
                .iter_mut()
                .find(|h| h.phase == phase && h.name == hook.name)
                .unwrap();
            h.finished_at = Some(now());
            match &result {
                Ok(skip) => {
                    h.status = if *skip { "skipped" } else { "succeeded" }.into();
                    h.reason = skip.then(|| "already_satisfied".into());
                    h.exit_code = (!skip).then_some(0);
                }
                Err(e) => {
                    h.status = if e.cancelled { "cancelled" } else { "failed" }.into();
                    h.exit_code = e.code;
                    h.error = (!e.cancelled).then(|| e.message.clone());
                    h.reason = e.cancelled.then(|| "cancelled".into());
                    i.state = if e.cancelled {
                        InitializationState::Cancelled
                    } else {
                        InitializationState::Failed
                    };
                    for later in &mut i.hooks {
                        if later.status == "pending" {
                            later.status = "skipped".into();
                            later.reason = Some(
                                if e.cancelled {
                                    "cancelled"
                                } else {
                                    "prior_failure"
                                }
                                .into(),
                            );
                            later.finished_at = Some(now());
                        }
                    }
                }
            }
        })
        .await;
        let stage = crate::daemon::with_process(state, handle, |r| {
            r.initialization
                .hooks
                .iter()
                .find(|h| h.phase == phase && h.name == hook.name)
                .and_then(|h| h.stage.clone())
        })
        .await
        .flatten()
        .unwrap_or_else(|| "checking".into());
        if result.as_ref().is_err_and(|e| !e.cancelled) {
            crate::daemon::initialization_failed(state).await;
        }
        step.log(&stage, &message).await;
        if result.is_err() {
            return false;
        }
    }
    if *cancel.borrow() {
        end_incarnation(state, handle, false).await;
        false
    } else {
        true
    }
}
