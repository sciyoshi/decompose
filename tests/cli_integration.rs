#![cfg(not(windows))]

mod support;
use nix::{
    sys::signal::{Signal, kill},
    unistd::Pid,
};
use support::process_alive;

use std::fs;
use std::io::{self, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::Duration;

use serde_json::Value;
use tempfile::tempdir;

fn bin_path() -> &'static str {
    env!("CARGO_BIN_EXE_decompose")
}

fn setup_project() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf, PathBuf) {
    let root = tempdir().expect("tempdir");
    let project = root.path().join("project");
    let runtime = root.path().join("runtime");
    let state = root.path().join("state");
    let home = root.path().join("home");
    fs::create_dir_all(&project).expect("create project");
    fs::create_dir_all(&runtime).expect("create runtime");
    fs::create_dir_all(&state).expect("create state");
    fs::create_dir_all(&home).expect("create home");

    let cfg = project.join("decompose.yaml");
    fs::write(
        &cfg,
        r#"
processes:
  sleeper:
    command: "sleep 30"
"#,
    )
    .expect("write config");

    (root, project, runtime, state, cfg)
}

fn run_cmd(
    project: &Path,
    runtime: &Path,
    state: &Path,
    home: &Path,
    args: &[&str],
    set_env: &[(&str, &str)],
    remove_env: &[&str],
) -> Output {
    let mut cmd = Command::new(bin_path());
    cmd.current_dir(project)
        .env("XDG_RUNTIME_DIR", runtime)
        .env("XDG_STATE_HOME", state)
        .env("HOME", home)
        .args(args);

    for (k, v) in set_env {
        cmd.env(k, v);
    }
    for key in remove_env {
        cmd.env_remove(key);
    }

    cmd.output().expect("command output")
}

fn assert_success(output: &Output, context: &str) {
    if !output.status.success() {
        panic!(
            "{context} failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn daemon_replies_to_invalid_requests_and_remains_responsive() {
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::UnixStream;

    let (root, project, runtime, state, _) = setup_project();
    let home = root.path().join("home");
    let run = |args: &[&str]| run_cmd(&project, &runtime, &state, &home, args, &[], &[]);
    assert_success(&run(&["up", "-d"]), "up");

    // Always shut down the test daemon, including when a socket check fails.
    let checks = std::panic::catch_unwind(|| {
        let socket = fs::read_dir(runtime.join("decompose"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.extension().is_some_and(|ext| ext == "sock"))
            .expect("daemon socket");
        let request = |payload: &str| {
            let mut stream = UnixStream::connect(&socket).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            writeln!(stream, "{payload}").unwrap();
            let mut line = String::new();
            BufReader::new(stream).read_line(&mut line).unwrap();
            serde_json::from_str::<Value>(&line).expect("JSON response")
        };

        for (payload, detail) in [
            (r#"{"type":"future_command"}"#, "unknown variant"),
            ("not json", "expected"),
            (r#"{"type":"stop"}"#, "missing field"),
            (r#"{"type":"down","timeout_seconds":-1}"#, "invalid value"),
        ] {
            let response = request(payload);
            assert_eq!(response["type"], "error");
            let message = response["message"].as_str().expect("error message");
            assert!(message.contains("invalid request json"), "{message}");
            assert!(message.contains(detail), "{message}");
            assert_eq!(request(r#"{"type":"ping"}"#)["type"], "pong");
        }
        // Extra fields from newer clients remain accepted on known commands.
        assert_eq!(
            request(r#"{"type":"ping","future_field":true}"#)["type"],
            "pong"
        );
    });
    assert_success(&run(&["down"]), "down after invalid requests");
    if let Err(panic) = checks {
        std::panic::resume_unwind(panic);
    }
}

fn spawn_http_ok_server() -> (u16, Arc<AtomicBool>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind ephemeral port");
    let port = listener
        .local_addr()
        .expect("ephemeral port local addr")
        .port();
    listener
        .set_nonblocking(true)
        .expect("set test server nonblocking");

    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let handle = thread::spawn(move || {
        while !thread_stop.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((mut stream, _addr)) => {
                    let _ = stream.write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                    );
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
    });

    (port, stop, handle)
}

/// Fluent test fixture that wraps `setup_project` + `run_cmd` with:
///   * shared temp directory and XDG paths,
///   * a helper for (re)writing the project's `decompose.yaml`,
///   * convenience wrappers around `up`, `down`, `ps`, `logs`, `run_args`,
///   * automatic `down` on drop so panicking tests don't leak daemons.
///
/// Tests that need something exotic (attached `up`, raw `Command`, custom
/// env vars, etc.) can still use the underlying `run_cmd` via `env.run_cmd(...)`
/// or reach for the fields directly.
struct TestEnv {
    _root: tempfile::TempDir,
    project: PathBuf,
    runtime: PathBuf,
    state: PathBuf,
    home: PathBuf,
    cfg_path: PathBuf,
    up_started: bool,
}

impl TestEnv {
    fn new() -> Self {
        let (root, project, runtime, state, cfg_path) = setup_project();
        let home = project.parent().expect("parent").join("home");
        Self {
            _root: root,
            project,
            runtime,
            state,
            home,
            cfg_path,
            up_started: false,
        }
    }

    fn cfg_arg(&self) -> String {
        self.cfg_path.to_string_lossy().to_string()
    }

    /// Overwrite `decompose.yaml` with the provided contents.
    fn with_config(&mut self, contents: &str) -> &mut Self {
        fs::write(&self.cfg_path, contents).expect("write config");
        self
    }

    /// Run the binary with `--file <cfg>` prepended to the given args.
    /// Use this for commands that take a config (up, down, ps, logs, ...).
    fn run(&self, args: &[&str]) -> Output {
        let cfg = self.cfg_arg();
        let mut full = Vec::with_capacity(args.len() + 2);
        full.push("--file");
        full.push(&cfg);
        full.extend_from_slice(args);
        run_cmd(
            &self.project,
            &self.runtime,
            &self.state,
            &self.home,
            &full,
            &[],
            &[],
        )
    }

    fn up_detach_json(&mut self) -> Output {
        let out = self.run(&["up", "--detach", "--json"]);
        assert_success(&out, "up --detach --json");
        self.up_started = true;
        out
    }

    fn ps_json(&self) -> Output {
        let out = self.run(&["ps", "--json"]);
        assert_success(&out, "ps --json");
        out
    }

    fn ps_json_value(&self) -> Value {
        let out = self.ps_json();
        serde_json::from_slice(&out.stdout).expect("ps json")
    }

    fn down_json(&mut self) -> Output {
        let out = self.run(&["down", "--json"]);
        assert_success(&out, "down --json");
        self.up_started = false;
        out
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        if !self.up_started {
            return;
        }
        // Best-effort cleanup: never panic from Drop (esp. mid-unwind).
        let _ = run_cmd(
            &self.project,
            &self.runtime,
            &self.state,
            &self.home,
            &["--file", &self.cfg_arg(), "down", "--json"],
            &[],
            &[],
        );
    }
}

#[test]
fn cli_supports_json_and_table_modes() {
    let mut env = TestEnv::new();

    let up = env.up_detach_json();
    let up_json: Value = serde_json::from_slice(&up.stdout).expect("up json");
    assert_eq!(
        up_json.get("daemon_action").and_then(Value::as_str),
        Some("started")
    );

    let parsed = env.ps_json_value();
    assert!(parsed.get("processes").and_then(Value::as_array).is_some());

    let ps_table = env.run(&["ps", "--table"]);
    assert_success(&ps_table, "ps --table");
    let ps_table_text = String::from_utf8_lossy(&ps_table.stdout);
    assert!(ps_table_text.contains("name"));
    assert!(ps_table_text.contains("sleeper"));

    let down = env.down_json();
    let down_json: Value = serde_json::from_slice(&down.stdout).expect("down json");
    assert_eq!(
        down_json.get("outcome").and_then(Value::as_str),
        Some("completed")
    );
}

#[test]
fn default_output_mode_is_text_with_or_without_ci_and_llm() {
    let (_root, project, runtime, state, config) = setup_project();
    let home = project.parent().expect("parent").join("home");
    let cfg = config.to_string_lossy().to_string();

    let up = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "up", "--detach", "--json"],
        &[],
        &[],
    );
    assert_success(&up, "up");

    let ps_default_table = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "ps"],
        &[("CI", "true")],
        &["LLM"],
    );
    assert_success(&ps_default_table, "default table ps");
    let table_text = String::from_utf8_lossy(&ps_default_table.stdout);
    assert!(table_text.contains("name"));

    let ps_default_json = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "ps"],
        &[],
        &["CI", "LLM"],
    );
    assert_success(&ps_default_json, "default json ps");
    assert!(String::from_utf8_lossy(&ps_default_json.stdout).contains("daemon running"));

    let down = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "down", "--json"],
        &[],
        &[],
    );
    assert_success(&down, "down");
}

#[test]
fn ctrl_c_stops_owned_environment() {
    let (_root, project, runtime, state, config) = setup_project();
    let home = project.parent().expect("parent").join("home");
    let cfg = config.to_string_lossy().to_string();

    let mut up = Command::new(bin_path());
    up.current_dir(&project)
        .env("XDG_RUNTIME_DIR", &runtime)
        .env("XDG_STATE_HOME", &state)
        .env("HOME", &home)
        .arg("--file")
        .arg(&cfg)
        .arg("up")
        .arg("--table")
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    let mut child = up.spawn().expect("spawn attached up");
    thread::sleep(Duration::from_millis(1500));

    kill(Pid::from_raw(child.id() as i32), Signal::SIGINT).expect("send ctrl-c");

    let up_exit = child.wait().expect("wait up");
    assert!(up_exit.success(), "up should stop cleanly");

    let ps = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "ps", "--json"],
        &[],
        &[],
    );
    assert_success(&ps, "ps after ctrl-c shutdown");
    let value: Value = serde_json::from_slice(&ps.stdout).unwrap();
    assert!(value["processes"].as_array().unwrap().is_empty());

    let down = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "down", "--json"],
        &[],
        &[],
    );
    assert_success(&down, "down after ctrl-c detach");
}

#[test]
fn top_level_stop_start_restart_target_services() {
    let (_root, project, runtime, state, _config) = setup_project();
    let home = project.parent().expect("parent").join("home");

    // Use two long-lived processes so we can target individually.
    let cfg_path = project.join("decompose.yaml");
    fs::write(
        &cfg_path,
        r#"
processes:
  alpha:
    command: "sleep 30"
  beta:
    command: "sleep 30"
"#,
    )
    .expect("write config");
    let cfg = cfg_path.to_string_lossy().to_string();

    let up = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "up", "--detach", "--json"],
        &[],
        &[],
    );
    assert_success(&up, "up");

    // Top-level stop with a specific service.
    let stop = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "stop", "--json", "alpha"],
        &[],
        &[],
    );
    assert_success(&stop, "stop alpha");
    let stop_json: Value = serde_json::from_slice(&stop.stdout).expect("stop json");
    assert_eq!(
        stop_json.get("outcome").and_then(Value::as_str),
        Some("completed")
    );

    // Top-level stop with no args stops all remaining services.
    let stop_all = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "stop", "--json"],
        &[],
        &[],
    );
    assert_success(&stop_all, "stop all");

    // Unknown service name returns a non-zero exit with a clear error.
    let bad = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "stop", "--json", "no-such-service"],
        &[],
        &[],
    );
    assert!(!bad.status.success(), "unknown service should fail");
    let stderr = String::from_utf8_lossy(&bad.stderr);
    assert!(
        stderr.contains("unknown service"),
        "error should mention 'unknown service', got: {stderr}"
    );

    let down = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "down", "--json"],
        &[],
        &[],
    );
    assert_success(&down, "down");
}

#[test]
fn down_when_not_running_exits_zero() {
    let env = TestEnv::new();

    let down = env.run(&["down", "--json"]);
    assert_success(&down, "down when nothing is running");
    let parsed: Value = serde_json::from_slice(&down.stdout).expect("down json");
    assert_eq!(parsed["outcome"], "unchanged");
}

#[test]
fn ps_when_not_running_is_empty_not_error() {
    let env = TestEnv::new();

    let parsed = env.ps_json_value();
    assert_eq!(parsed["daemon"]["state"], "not_running");
    assert_eq!(
        parsed
            .get("processes")
            .and_then(Value::as_array)
            .map(std::vec::Vec::len),
        Some(0)
    );

    let ps_table = env.run(&["ps", "--table"]);
    assert_success(&ps_table, "ps --table when not running");
    let table = String::from_utf8_lossy(&ps_table.stdout);
    assert!(table.contains("daemon not running"));
}

#[test]
fn config_prints_resolved_json() {
    let root = tempdir().expect("tempdir");
    let project = root.path().join("project");
    let runtime = root.path().join("runtime");
    let state = root.path().join("state");
    let home = root.path().join("home");
    fs::create_dir_all(&project).expect("create project");
    fs::create_dir_all(&runtime).expect("create runtime");
    fs::create_dir_all(&state).expect("create state");
    fs::create_dir_all(&home).expect("create home");

    let cfg_path = project.join("decompose.yaml");
    fs::write(
        &cfg_path,
        r#"
processes:
  web:
    command: "node ${ENTRYPOINT}"
    environment:
      ENTRYPOINT: server.js
  worker:
    command: "python worker.py"
"#,
    )
    .expect("write config");
    let cfg = cfg_path.to_string_lossy().to_string();

    let out = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "config", "--json"],
        &[],
        &[],
    );
    assert_success(&out, "config --json");
    let parsed: Value = serde_json::from_slice(&out.stdout).expect("config json");
    let procs = parsed.get("processes").expect("has processes field");
    assert!(procs.get("web").is_some(), "contains web process");
    assert!(procs.get("worker").is_some(), "contains worker process");
    assert_eq!(
        procs
            .get("web")
            .and_then(|p| p.get("command"))
            .and_then(Value::as_str),
        Some("node server.js")
    );

    let out_yaml = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "config", "--table"],
        &[],
        &[],
    );
    assert_success(&out_yaml, "config --table (yaml)");
    let yaml_text = String::from_utf8_lossy(&out_yaml.stdout);
    assert!(yaml_text.contains("web"), "yaml contains web");
    assert!(yaml_text.contains("worker"), "yaml contains worker");
    assert!(
        yaml_text.contains("node server.js"),
        "yaml should contain interpolated command, got: {yaml_text}"
    );
}

#[test]
fn explicit_config_paths_resolve_service_paths_from_config_dir() {
    let root = tempdir().expect("tempdir");
    let project = root.path().join("project");
    let caller = root.path().join("caller");
    let app = project.join("app");
    let runtime = root.path().join("runtime");
    let state = root.path().join("state");
    let home = root.path().join("home");
    fs::create_dir_all(&app).expect("create app");
    fs::create_dir_all(&caller).expect("create caller");
    fs::create_dir_all(&runtime).expect("create runtime");
    fs::create_dir_all(&state).expect("create state");
    fs::create_dir_all(&home).expect("create home");

    fs::write(project.join(".env"), "BASE=from_dotenv\n").expect("write dotenv");
    fs::write(project.join("service.env"), "EXTRA=from_env_file\n").expect("write env file");
    fs::write(
        project.join("decompose.yaml"),
        r#"
processes:
  writer:
    command: "sh -c 'printf \"$$PWD|$$BASE|$$EXTRA\" > up.txt; sleep 30'"
    working_dir: app
    env_file:
      - service.env
"#,
    )
    .expect("write config");

    let cfg = "../project/decompose.yaml";
    let run = run_cmd(
        &caller,
        &runtime,
        &state,
        &home,
        &[
            "--file",
            cfg,
            "run",
            "writer",
            "sh",
            "-c",
            "printf \"$PWD|$BASE|$EXTRA\" > run.txt",
        ],
        &[],
        &[],
    );
    assert_success(&run, "run from outside config dir");

    let canonical_app = app.canonicalize().expect("canonical app path");
    let expected = format!("{}|from_dotenv|from_env_file", canonical_app.display());
    let run_out = fs::read_to_string(app.join("run.txt")).expect("read run output");
    assert_eq!(run_out, expected);

    let up = run_cmd(
        &caller,
        &runtime,
        &state,
        &home,
        &["--file", cfg, "up", "--detach", "--json"],
        &[],
        &[],
    );
    assert_success(&up, "up from outside config dir");

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !app.join("up.txt").exists() && std::time::Instant::now() < deadline {
        thread::sleep(Duration::from_millis(100));
    }
    let up_out = fs::read_to_string(app.join("up.txt")).expect("read up output");
    assert_eq!(up_out, expected);

    let down = run_cmd(
        &caller,
        &runtime,
        &state,
        &home,
        &["--file", cfg, "down", "--json"],
        &[],
        &[],
    );
    assert_success(&down, "down from outside config dir");
}

#[test]
fn config_errors_on_invalid_yaml() {
    let root = tempdir().expect("tempdir");
    let project = root.path().join("project");
    let runtime = root.path().join("runtime");
    let state = root.path().join("state");
    let home = root.path().join("home");
    fs::create_dir_all(&project).expect("create project");
    fs::create_dir_all(&runtime).expect("create runtime");
    fs::create_dir_all(&state).expect("create state");
    fs::create_dir_all(&home).expect("create home");

    let cfg_path = project.join("decompose.yaml");
    fs::write(&cfg_path, "not: valid: yaml: [[[").expect("write bad config");
    let cfg = cfg_path.to_string_lossy().to_string();

    let out = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "config", "--json"],
        &[],
        &[],
    );
    assert!(!out.status.success(), "config should fail on invalid yaml");
}

#[test]
fn kill_sends_signal_to_running_service() {
    let (_root, project, runtime, state, _config) = setup_project();
    let home = project.parent().expect("parent").join("home");

    let cfg_path = project.join("decompose.yaml");
    fs::write(
        &cfg_path,
        r#"
processes:
  sleeper:
    command: "sleep 30"
"#,
    )
    .expect("write config");
    let cfg = cfg_path.to_string_lossy().to_string();

    let up = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "up", "--detach", "--json"],
        &[],
        &[],
    );
    assert_success(&up, "up");

    thread::sleep(Duration::from_millis(500));

    let kill = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "kill", "--json", "sleeper"],
        &[],
        &[],
    );
    assert_success(&kill, "kill sleeper");
    let kill_json: Value = serde_json::from_slice(&kill.stdout).expect("kill json");
    assert_eq!(
        kill_json.get("outcome").and_then(Value::as_str),
        Some("completed")
    );

    thread::sleep(Duration::from_millis(500));

    let ps = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "ps", "--json"],
        &[],
        &[],
    );
    assert_success(&ps, "ps after kill");
    let ps_json: Value = serde_json::from_slice(&ps.stdout).expect("ps json");
    let processes = ps_json
        .get("processes")
        .and_then(Value::as_array)
        .expect("processes array");
    let sleeper = processes
        .iter()
        .find(|p| p.get("name").and_then(Value::as_str) == Some("sleeper"))
        .expect("sleeper process");
    let state_str = sleeper.get("state").and_then(Value::as_str).unwrap_or("");
    assert!(
        state_str == "exited" || state_str == "failed" || state_str == "stopped",
        "expected exited, failed, or stopped, got: {state_str}"
    );

    let down = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "down", "--json"],
        &[],
        &[],
    );
    assert_success(&down, "down");
}

#[test]
fn ls_lists_running_environments() {
    let (_root, project, runtime, state, config) = setup_project();
    let home = project.parent().expect("parent").join("home");
    let cfg = config.to_string_lossy().to_string();

    let up = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "up", "--detach", "--json"],
        &[],
        &[],
    );
    assert_success(&up, "up");

    let ls = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["ls", "--json"],
        &[],
        &[],
    );
    assert_success(&ls, "ls --json");
    let parsed: Value = serde_json::from_slice(&ls.stdout).expect("ls json");
    let envs = parsed
        .get("environments")
        .and_then(Value::as_array)
        .expect("environments array");
    assert!(!envs.is_empty(), "should have at least one environment");
    assert_eq!(
        envs[0]["daemon"].get("state").and_then(Value::as_str),
        Some("running")
    );

    let ls_table = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["ls", "--table"],
        &[],
        &[],
    );
    assert_success(&ls_table, "ls --table");
    let table_text = String::from_utf8_lossy(&ls_table.stdout);
    assert!(table_text.contains("instance"));
    assert!(table_text.contains("running"));

    let down = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "down", "--json"],
        &[],
        &[],
    );
    assert_success(&down, "down");
}

#[test]
fn cycle_detection_simple_two_node_cycle() {
    let (_root, project, runtime, state, _config) = setup_project();
    let home = project.parent().expect("parent").join("home");

    let cfg_path = project.join("decompose.yaml");
    fs::write(
        &cfg_path,
        r#"
processes:
  a:
    command: "sleep 1"
    depends_on:
      b:
        condition: process_started
  b:
    command: "sleep 1"
    depends_on:
      a:
        condition: process_started
"#,
    )
    .expect("write config");
    let cfg = cfg_path.to_string_lossy().to_string();

    let up = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "up", "--detach", "--json"],
        &[],
        &[],
    );
    assert!(
        !up.status.success(),
        "up should fail with a dependency cycle"
    );
    let stderr = String::from_utf8_lossy(&up.stderr);
    assert!(
        stderr.contains("dependency cycle detected"),
        "stderr should mention cycle, got: {stderr}"
    );
}

#[test]
fn cycle_detection_three_node_cycle() {
    let (_root, project, runtime, state, _config) = setup_project();
    let home = project.parent().expect("parent").join("home");

    let cfg_path = project.join("decompose.yaml");
    fs::write(
        &cfg_path,
        r#"
processes:
  a:
    command: "sleep 1"
    depends_on:
      b:
        condition: process_started
  b:
    command: "sleep 1"
    depends_on:
      c:
        condition: process_started
  c:
    command: "sleep 1"
    depends_on:
      a:
        condition: process_started
"#,
    )
    .expect("write config");
    let cfg = cfg_path.to_string_lossy().to_string();

    let up = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "up", "--detach", "--json"],
        &[],
        &[],
    );
    assert!(
        !up.status.success(),
        "up should fail with a three-node dependency cycle"
    );
    let stderr = String::from_utf8_lossy(&up.stderr);
    assert!(
        stderr.contains("dependency cycle detected"),
        "stderr should mention cycle, got: {stderr}"
    );
}

#[test]
fn cycle_detection_self_dependency() {
    let (_root, project, runtime, state, _config) = setup_project();
    let home = project.parent().expect("parent").join("home");

    let cfg_path = project.join("decompose.yaml");
    fs::write(
        &cfg_path,
        r#"
processes:
  a:
    command: "sleep 1"
    depends_on:
      a:
        condition: process_started
"#,
    )
    .expect("write config");
    let cfg = cfg_path.to_string_lossy().to_string();

    let up = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "up", "--detach", "--json"],
        &[],
        &[],
    );
    assert!(
        !up.status.success(),
        "up should fail with a self-dependency cycle"
    );
    let stderr = String::from_utf8_lossy(&up.stderr);
    assert!(
        stderr.contains("dependency cycle detected"),
        "stderr should mention cycle, got: {stderr}"
    );
}

#[test]
fn cycle_detection_valid_dag_succeeds() {
    let (_root, project, runtime, state, _config) = setup_project();
    let home = project.parent().expect("parent").join("home");

    let cfg_path = project.join("decompose.yaml");
    fs::write(
        &cfg_path,
        r#"
processes:
  a:
    command: "sleep 30"
    depends_on:
      b:
        condition: process_started
  b:
    command: "sleep 30"
    depends_on:
      c:
        condition: process_started
  c:
    command: "sleep 30"
"#,
    )
    .expect("write config");
    let cfg = cfg_path.to_string_lossy().to_string();

    let up = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "up", "--detach", "--json"],
        &[],
        &[],
    );
    assert_success(&up, "up with valid DAG (no cycle)");

    let down = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "down", "--json"],
        &[],
        &[],
    );
    assert_success(&down, "down after valid DAG");
}

#[test]
fn down_with_timeout_flag() {
    let (_root, project, runtime, state, config) = setup_project();
    let home = project.parent().expect("parent").join("home");
    let cfg = config.to_string_lossy().to_string();

    let up = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "up", "--detach", "--json"],
        &[],
        &[],
    );
    assert_success(&up, "up");

    // Give the daemon a moment to start processes.
    thread::sleep(Duration::from_millis(500));

    let down = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "down", "--timeout", "1", "--json"],
        &[],
        &[],
    );
    assert_success(&down, "down --timeout 1");
    let down_json: Value = serde_json::from_slice(&down.stdout).expect("down json");
    assert_eq!(
        down_json.get("outcome").and_then(Value::as_str),
        Some("completed")
    );
}

#[test]
fn restart_on_failure_increments_restart_count() {
    let (_root, project, runtime, state, _config) = setup_project();
    let home = project.parent().expect("parent").join("home");

    let cfg_path = project.join("decompose.yaml");
    fs::write(
        &cfg_path,
        r#"
processes:
  failer:
    command: "sh -c 'sleep 0.5; exit 1'"
    restart_policy: on_failure
    backoff_seconds: 1
"#,
    )
    .expect("write config");
    let cfg = cfg_path.to_string_lossy().to_string();

    let up = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "up", "--detach", "--json"],
        &[],
        &[],
    );
    assert_success(&up, "up");

    // Wait long enough for at least one restart cycle:
    // initial run (~0.5s) + backoff (1s) + second run (~0.5s) + buffer
    thread::sleep(Duration::from_secs(4));

    let ps = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "ps", "--json"],
        &[],
        &[],
    );
    assert_success(&ps, "ps after restart");
    let ps_json: Value = serde_json::from_slice(&ps.stdout).expect("ps json");
    let processes = ps_json
        .get("processes")
        .and_then(Value::as_array)
        .expect("processes array");
    let failer = processes
        .iter()
        .find(|p| p.get("name").and_then(Value::as_str) == Some("failer"))
        .expect("failer process");
    let restart_count = failer
        .get("restart_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    assert!(
        restart_count > 0,
        "expected restart_count > 0 after failure, got: {restart_count}"
    );

    let down = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "down", "--json"],
        &[],
        &[],
    );
    assert_success(&down, "down");
}

#[test]
fn max_restarts_caps_restart_count() {
    let (_root, project, runtime, state, _config) = setup_project();
    let home = project.parent().expect("parent").join("home");

    let cfg_path = project.join("decompose.yaml");
    fs::write(
        &cfg_path,
        r#"
processes:
  capped:
    command: "sh -c 'sleep 0.3; exit 1'"
    restart_policy: on_failure
    backoff_seconds: 1
    max_restarts: 2
"#,
    )
    .expect("write config");
    let cfg = cfg_path.to_string_lossy().to_string();

    let up = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "up", "--detach", "--json"],
        &[],
        &[],
    );
    assert_success(&up, "up");

    // Wait for all restarts to exhaust:
    // initial run (~0.3s) + backoff (1s) + restart 1 (~0.3s) + backoff (1s)
    // + restart 2 (~0.3s) = ~2.9s, use generous buffer
    thread::sleep(Duration::from_secs(6));

    let ps = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "ps", "--json"],
        &[],
        &[],
    );
    assert_success(&ps, "ps after max restarts exhausted");
    let ps_json: Value = serde_json::from_slice(&ps.stdout).expect("ps json");
    let processes = ps_json
        .get("processes")
        .and_then(Value::as_array)
        .expect("processes array");
    let capped = processes
        .iter()
        .find(|p| p.get("name").and_then(Value::as_str) == Some("capped"))
        .expect("capped process");
    let restart_count = capped
        .get("restart_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    assert!(
        restart_count <= 2,
        "expected restart_count <= 2 (max_restarts cap), got: {restart_count}"
    );
    assert_eq!(
        restart_count, 2,
        "expected exactly 2 restarts before stopping"
    );

    // The process should be in a terminal state (failed) after exhausting restarts.
    let state_str = capped.get("state").and_then(Value::as_str).unwrap_or("");
    assert_eq!(
        state_str, "failed",
        "expected process to be in 'failed' state after exhausting restarts, got: {state_str}"
    );

    let down = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "down", "--json"],
        &[],
        &[],
    );
    assert_success(&down, "down");
}

#[test]
fn restart_separator_appears_in_logs_between_runs() {
    let (_root, project, runtime, state, _config) = setup_project();
    let home = project.parent().expect("parent").join("home");

    let cfg_path = project.join("decompose.yaml");
    fs::write(
        &cfg_path,
        r#"
processes:
  flaky:
    command: "sh -c 'echo TICK; exit 1'"
    restart_policy: always
    backoff_seconds: 1
    max_restarts: 2
"#,
    )
    .expect("write config");
    let cfg = cfg_path.to_string_lossy().to_string();

    let up = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "up", "--detach", "--json"],
        &[],
        &[],
    );
    assert_success(&up, "up");

    // Initial run + backoff 1s + restart 1 + backoff 1s + restart 2 = ~2-3s.
    // Give a generous buffer for CI noise.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut saw_separator = false;
    let mut last_logs = String::new();
    while std::time::Instant::now() < deadline {
        let logs = run_cmd(
            &project,
            &runtime,
            &state,
            &home,
            &["--file", &cfg, "logs", "--no-pager"],
            &[("DECOMPOSE_PAGER", "false")],
            &[],
        );
        assert_success(&logs, "logs --no-pager");
        last_logs = String::from_utf8_lossy(&logs.stdout).to_string();
        // Look for the separator line with the expected shape.
        if last_logs.contains("[flaky] --- restarted (exit code 1, attempt 1/2) ---")
            || last_logs.contains("[flaky] --- restarted (exit code 1, attempt 2/2) ---")
        {
            saw_separator = true;
            break;
        }
        thread::sleep(Duration::from_millis(200));
    }

    let down = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "down", "--json"],
        &[],
        &[],
    );
    assert_success(&down, "down");

    assert!(
        saw_separator,
        "expected a `[flaky] --- restarted (exit code 1, attempt N/2) ---` line in the daemon log, got:\n{last_logs}"
    );
}

#[test]
fn no_restart_on_successful_exit() {
    let (_root, project, runtime, state, _config) = setup_project();
    let home = project.parent().expect("parent").join("home");

    let cfg_path = project.join("decompose.yaml");
    fs::write(
        &cfg_path,
        r#"
processes:
  succeeder:
    command: "sh -c 'sleep 0.3; exit 0'"
    restart_policy: on_failure
    backoff_seconds: 1
"#,
    )
    .expect("write config");
    let cfg = cfg_path.to_string_lossy().to_string();

    let up = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "up", "--detach", "--json"],
        &[],
        &[],
    );
    assert_success(&up, "up");

    // Wait long enough for the process to exit and for any hypothetical
    // restart to have happened.
    thread::sleep(Duration::from_secs(3));

    let ps = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "ps", "--json"],
        &[],
        &[],
    );
    assert_success(&ps, "ps after successful exit");
    let ps_json: Value = serde_json::from_slice(&ps.stdout).expect("ps json");
    let processes = ps_json
        .get("processes")
        .and_then(Value::as_array)
        .expect("processes array");
    let succeeder = processes
        .iter()
        .find(|p| p.get("name").and_then(Value::as_str) == Some("succeeder"))
        .expect("succeeder process");
    let restart_count = succeeder
        .get("restart_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    assert_eq!(
        restart_count, 0,
        "expected no restarts for a successfully exiting process with on_failure policy"
    );

    // Should be in exited state (exit code 0 -> "exited" in to_json_status).
    let state_str = succeeder.get("state").and_then(Value::as_str).unwrap_or("");
    assert_eq!(
        state_str, "exited",
        "expected process to be in 'exited' state, got: {state_str}"
    );

    let down = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "down", "--json"],
        &[],
        &[],
    );
    assert_success(&down, "down");
}

#[test]
fn up_detach_wait_returns_when_services_running() {
    let mut env = TestEnv::new();

    let up = env.run(&["up", "-d", "--wait", "--json"]);
    assert_success(&up, "up -d --wait");
    env.up_started = true;

    env.down_json();
}

#[test]
fn up_detach_wait_fails_when_service_fails() {
    let mut env = TestEnv::new();
    env.with_config(
        r#"
processes:
  failer:
    command: "sh -c 'exit 1'"
"#,
    );

    let up = env.run(&["up", "-d", "--wait", "--json"]);
    env.up_started = true;
    assert!(!up.status.success(), "up -d --wait should fail");

    assert!(up.stdout.is_empty());
    let diagnostic: Value = serde_json::from_slice(&up.stderr).expect("one diagnostic");
    assert_eq!(diagnostic["code"], "readiness_failed");
    assert_eq!(diagnostic["details"]["daemon"]["state"], "running");

    env.down_json();
}

#[test]
fn up_wait_requires_detach() {
    let env = TestEnv::new();

    let up = env.run(&["up", "--wait", "--json"]);
    assert!(!up.status.success(), "up --wait without -d should fail");

    let stderr = String::from_utf8_lossy(&up.stderr);
    assert!(
        stderr.contains("--detach") || stderr.contains("-d"),
        "stderr should mention detach requirement, got: {stderr}"
    );
}

#[test]
fn shutdown_normal_sigterm_clean_exit() {
    let mut env = TestEnv::new();
    env.with_config(
        r#"
processes:
  trapper:
    command: "sh -c 'trap \"exit 0\" TERM; sleep 30'"
"#,
    );

    env.up_detach_json();

    // Give the process time to start and register the trap.
    thread::sleep(Duration::from_millis(500));

    let down = env.down_json();
    let down_json: Value = serde_json::from_slice(&down.stdout).expect("down json");
    assert_eq!(
        down_json.get("outcome").and_then(Value::as_str),
        Some("completed")
    );
}

#[test]
fn shutdown_timeout_escalation_to_sigkill() {
    let mut env = TestEnv::new();
    // Process that ignores SIGTERM, so shutdown must escalate to SIGKILL.
    env.with_config(
        r#"
processes:
  stubborn:
    command: "sh -c 'trap \"\" TERM; sleep 30'"
    shutdown:
      timeout_seconds: 1
"#,
    );

    env.up_detach_json();

    // Give the process time to start and register the trap.
    thread::sleep(Duration::from_millis(500));

    let start = std::time::Instant::now();
    let down = env.run(&["down", "--timeout", "1", "--json"]);
    let elapsed = start.elapsed();

    assert_success(&down, "down after timeout escalation to SIGKILL");
    env.up_started = false;
    let down_json: Value = serde_json::from_slice(&down.stdout).expect("down json");
    assert_eq!(
        down_json.get("outcome").and_then(Value::as_str),
        Some("completed")
    );

    // The process ignores SIGTERM so must wait for the 1-second timeout
    // before SIGKILL. Verify it didn't take longer than 10 seconds (generous
    // upper bound to avoid flakiness).
    assert!(
        elapsed < Duration::from_secs(10),
        "down should complete quickly after SIGKILL, took {:?}",
        elapsed
    );
}

#[test]
fn shutdown_custom_signal() {
    let mut env = TestEnv::new();
    // Process that traps SIGINT (signal 2) and exits cleanly, but ignores SIGTERM.
    env.with_config(
        r#"
processes:
  custom_sig:
    command: "sh -c 'trap \"exit 0\" INT; trap \"\" TERM; sleep 30'"
    shutdown:
      signal: 2
      timeout_seconds: 5
"#,
    );

    env.up_detach_json();

    // Give the process time to start and register the traps.
    thread::sleep(Duration::from_millis(500));

    let start = std::time::Instant::now();
    let down = env.down_json();
    let elapsed = start.elapsed();

    let down_json: Value = serde_json::from_slice(&down.stdout).expect("down json");
    assert_eq!(
        down_json.get("outcome").and_then(Value::as_str),
        Some("completed")
    );

    // With the custom signal (SIGINT) handled, the process should exit promptly
    // without needing the 5-second timeout escalation to SIGKILL.
    assert!(
        elapsed < Duration::from_secs(5),
        "down should exit quickly via custom signal, took {:?}",
        elapsed
    );
}
#[test]
fn two_sessions_coexist_independently() {
    let (_root, project, runtime, state, _config) = setup_project();
    let home = project.parent().expect("parent").join("home");

    // Write a config with two distinct processes so we can identify them.
    let cfg_path = project.join("decompose.yaml");
    fs::write(
        &cfg_path,
        r#"
processes:
  sleeper:
    command: "sleep 30"
"#,
    )
    .expect("write config");
    let cfg = cfg_path.to_string_lossy().to_string();

    // Start session alpha.
    let up_a = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "--session", "alpha", "up", "-d", "--json"],
        &[],
        &[],
    );
    assert_success(&up_a, "up --session alpha");

    // Start session beta.
    let up_b = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "--session", "beta", "up", "-d", "--json"],
        &[],
        &[],
    );
    assert_success(&up_b, "up --session beta");

    // Verify ps for alpha shows running processes.
    let ps_a = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "--session", "alpha", "ps", "--json"],
        &[],
        &[],
    );
    assert_success(&ps_a, "ps --session alpha");
    let ps_a_json: Value = serde_json::from_slice(&ps_a.stdout).expect("ps alpha json");
    let procs_a = ps_a_json
        .get("processes")
        .and_then(Value::as_array)
        .expect("alpha processes array");
    assert!(!procs_a.is_empty(), "alpha session should have processes");

    // Verify ps for beta shows running processes.
    let ps_b = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "--session", "beta", "ps", "--json"],
        &[],
        &[],
    );
    assert_success(&ps_b, "ps --session beta");
    let ps_b_json: Value = serde_json::from_slice(&ps_b.stdout).expect("ps beta json");
    let procs_b = ps_b_json
        .get("processes")
        .and_then(Value::as_array)
        .expect("beta processes array");
    assert!(!procs_b.is_empty(), "beta session should have processes");

    // Stop session alpha; beta should keep running.
    let down_a = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "--session", "alpha", "down", "--json"],
        &[],
        &[],
    );
    assert_success(&down_a, "down --session alpha");

    // Verify beta is still running after alpha is stopped.
    let ps_b2 = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "--session", "beta", "ps", "--json"],
        &[],
        &[],
    );
    assert_success(&ps_b2, "ps --session beta after alpha down");
    let ps_b2_json: Value = serde_json::from_slice(&ps_b2.stdout).expect("ps beta json 2");
    let procs_b2 = ps_b2_json
        .get("processes")
        .and_then(Value::as_array)
        .expect("beta processes array 2");
    assert!(
        !procs_b2.is_empty(),
        "beta session should still have processes after alpha is stopped"
    );

    // Clean up beta.
    let down_b = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "--session", "beta", "down", "--json"],
        &[],
        &[],
    );
    assert_success(&down_b, "down --session beta");
}

#[test]
fn session_isolation_from_default() {
    let (_root, project, runtime, state, _config) = setup_project();
    let home = project.parent().expect("parent").join("home");

    let cfg_path = project.join("decompose.yaml");
    fs::write(
        &cfg_path,
        r#"
processes:
  sleeper:
    command: "sleep 30"
"#,
    )
    .expect("write config");
    let cfg = cfg_path.to_string_lossy().to_string();

    // Start a named session.
    let up_foo = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "--session", "foo", "up", "-d", "--json"],
        &[],
        &[],
    );
    assert_success(&up_foo, "up --session foo");

    // The default session (no --session flag) should show nothing running.
    let ps_default = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "ps", "--json"],
        &[],
        &[],
    );
    assert_success(&ps_default, "ps default session");
    let ps_def_json: Value = serde_json::from_slice(&ps_default.stdout).expect("ps default json");
    assert_eq!(
        ps_def_json["daemon"].get("state").and_then(Value::as_str),
        Some("not_running"),
        "default session should not be running when only named session is up"
    );
    let procs_def = ps_def_json
        .get("processes")
        .and_then(Value::as_array)
        .expect("default processes array");
    assert!(
        procs_def.is_empty(),
        "default session should have no processes"
    );

    // Now start the default session too.
    let up_default = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "up", "-d", "--json"],
        &[],
        &[],
    );
    assert_success(&up_default, "up default session");

    // Both sessions should be independently running.
    let ps_foo = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "--session", "foo", "ps", "--json"],
        &[],
        &[],
    );
    assert_success(&ps_foo, "ps --session foo");
    let ps_foo_json: Value = serde_json::from_slice(&ps_foo.stdout).expect("ps foo json");
    let procs_foo = ps_foo_json
        .get("processes")
        .and_then(Value::as_array)
        .expect("foo processes array");
    assert!(
        !procs_foo.is_empty(),
        "foo session should have running processes"
    );

    let ps_def2 = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "ps", "--json"],
        &[],
        &[],
    );
    assert_success(&ps_def2, "ps default session after both up");
    let ps_def2_json: Value = serde_json::from_slice(&ps_def2.stdout).expect("ps default json 2");
    let procs_def2 = ps_def2_json
        .get("processes")
        .and_then(Value::as_array)
        .expect("default processes array 2");
    assert!(
        !procs_def2.is_empty(),
        "default session should have running processes"
    );

    // Clean up both sessions.
    let down_foo = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "--session", "foo", "down", "--json"],
        &[],
        &[],
    );
    assert_success(&down_foo, "down --session foo");

    let down_default = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "down", "--json"],
        &[],
        &[],
    );
    assert_success(&down_default, "down default session");
}
#[test]
fn ps_json_structure_has_all_expected_fields() {
    let (_root, project, runtime, state, config) = setup_project();
    let home = project.parent().expect("parent").join("home");
    let cfg = config.to_string_lossy().to_string();

    let up = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "up", "--detach", "--json"],
        &[],
        &[],
    );
    assert_success(&up, "up");

    // Give the daemon a moment to start the process.
    thread::sleep(Duration::from_millis(500));

    let ps = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "ps", "--json"],
        &[],
        &[],
    );
    assert_success(&ps, "ps --json");
    let parsed: Value = serde_json::from_slice(&ps.stdout).expect("ps json parse");

    // Top-level must have "processes" array.
    let processes = parsed
        .get("processes")
        .and_then(Value::as_array)
        .expect("top-level 'processes' array");
    assert!(!processes.is_empty(), "should have at least one process");

    // Verify each process snapshot has the expected fields with correct types.
    for proc in processes {
        let obj = proc.as_object().expect("process should be an object");

        // Required string fields.
        assert!(
            obj.get("name").and_then(Value::as_str).is_some(),
            "process must have string 'name', got: {proc}"
        );
        assert!(
            obj.get("state").and_then(Value::as_str).is_some(),
            "process must have string 'state', got: {proc}"
        );
        assert!(
            obj.get("status").and_then(Value::as_str).is_some(),
            "process must have string 'status', got: {proc}"
        );
        assert!(
            obj.get("base").and_then(Value::as_str).is_some(),
            "process must have string 'base', got: {proc}"
        );

        // Required boolean fields.
        assert!(
            obj.get("log_ready").and_then(Value::as_bool).is_some(),
            "process must have bool 'log_ready', got: {proc}"
        );
        assert!(
            obj.get("has_readiness_probe")
                .and_then(Value::as_bool)
                .is_some(),
            "process must have bool 'has_readiness_probe', got: {proc}"
        );

        // Required numeric fields.
        assert!(
            obj.get("restart_count").and_then(Value::as_u64).is_some(),
            "process must have numeric 'restart_count', got: {proc}"
        );
        assert!(
            obj.get("replica").and_then(Value::as_u64).is_some(),
            "process must have numeric 'replica', got: {proc}"
        );

        // Optional nullable fields must be present (even if null).
        assert!(
            obj.contains_key("pid"),
            "process must contain 'pid' key, got: {proc}"
        );
        assert!(
            obj.contains_key("exit_code"),
            "process must contain 'exit_code' key, got: {proc}"
        );
        assert!(
            obj.contains_key("description"),
            "process must contain 'description' key, got: {proc}"
        );
    }

    // Verify the specific sleeper process values.
    let sleeper = processes
        .iter()
        .find(|p| p.get("name").and_then(Value::as_str) == Some("sleeper"))
        .expect("should have a 'sleeper' process");
    assert_eq!(
        sleeper.get("state").and_then(Value::as_str),
        Some("running"),
        "sleeper should be in running state"
    );
    assert_eq!(
        sleeper.get("restart_count").and_then(Value::as_u64),
        Some(0),
        "sleeper restart_count should be 0"
    );

    let down = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "down", "--json"],
        &[],
        &[],
    );
    assert_success(&down, "down");
}

#[test]
fn up_json_structure_has_status_and_pid() {
    let (_root, project, runtime, state, config) = setup_project();
    let home = project.parent().expect("parent").join("home");
    let cfg = config.to_string_lossy().to_string();

    let up = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "up", "--detach", "--json"],
        &[],
        &[],
    );
    assert_success(&up, "up --json");
    let parsed: Value = serde_json::from_slice(&up.stdout).expect("up json parse");

    // Must have "status" string field.
    let status = parsed
        .get("daemon_action")
        .and_then(Value::as_str)
        .expect("up response must have string 'status'");
    assert_eq!(status, "started");

    // Must have "pid" numeric field.
    assert!(
        parsed["daemon"]
            .get("pid")
            .and_then(Value::as_u64)
            .is_some(),
        "up response must have numeric 'pid', got: {parsed}"
    );

    let down = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "down", "--json"],
        &[],
        &[],
    );
    assert_success(&down, "down");
}

#[test]
fn down_json_structure_has_status_ok() {
    let (_root, project, runtime, state, config) = setup_project();
    let home = project.parent().expect("parent").join("home");
    let cfg = config.to_string_lossy().to_string();

    // Start the daemon first.
    let up = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "up", "--detach", "--json"],
        &[],
        &[],
    );
    assert_success(&up, "up");

    let down = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "down", "--json"],
        &[],
        &[],
    );
    assert_success(&down, "down --json");
    let parsed: Value = serde_json::from_slice(&down.stdout).expect("down json parse");

    // Must have "status" string field with value "ok".
    let status = parsed
        .get("outcome")
        .and_then(Value::as_str)
        .expect("down response must have string 'status'");
    assert_eq!(status, "completed");
}

#[test]
fn ps_empty_json_structure_has_running_false_and_empty_processes() {
    let (_root, project, runtime, state, config) = setup_project();
    let home = project.parent().expect("parent").join("home");
    let cfg = config.to_string_lossy().to_string();

    let ps = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "ps", "--json"],
        &[],
        &[],
    );
    assert_success(&ps, "ps --json when not running");
    let parsed: Value = serde_json::from_slice(&ps.stdout).expect("ps json parse");

    assert_eq!(parsed["daemon"]["state"], "not_running");
    assert!(parsed["daemon"]["pid"].is_null());

    // Must have "processes" array that is empty.
    let processes = parsed
        .get("processes")
        .and_then(Value::as_array)
        .expect("empty ps response must have 'processes' array");
    assert!(
        processes.is_empty(),
        "processes should be empty when no daemon"
    );
}

#[test]
fn incremental_up_starts_second_service() {
    let (_root, project, runtime, state, _config) = setup_project();
    let home = project.parent().expect("parent").join("home");

    let cfg_path = project.join("decompose.yaml");
    fs::write(
        &cfg_path,
        r#"
processes:
  alpha:
    command: "sleep 30"
  beta:
    command: "sleep 30"
"#,
    )
    .expect("write config");
    let cfg = cfg_path.to_string_lossy().to_string();

    // Start only alpha
    let up1 = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "up", "-d", "--json", "alpha"],
        &[],
        &[],
    );
    assert_success(&up1, "up alpha");
    thread::sleep(Duration::from_millis(500));

    // ps should show alpha running and beta not_started
    let ps1 = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "ps", "--json"],
        &[],
        &[],
    );
    assert_success(&ps1, "ps after up alpha");
    let parsed: Value = serde_json::from_slice(&ps1.stdout).expect("ps json");
    let procs = parsed.get("processes").and_then(Value::as_array).unwrap();
    assert_eq!(procs.len(), 2, "should see both services in ps");
    let beta_state = procs
        .iter()
        .find(|p| p.get("name").and_then(Value::as_str) == Some("beta"))
        .and_then(|p| p.get("state").and_then(Value::as_str));
    assert_eq!(
        beta_state,
        Some("not_started"),
        "beta should be not_started"
    );

    // Now run `up -d beta` against the running daemon
    let up2 = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "up", "-d", "--json", "beta"],
        &[],
        &[],
    );
    assert_success(&up2, "up beta (incremental)");
    thread::sleep(Duration::from_millis(500));

    // Both should now be running
    let ps2 = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "ps", "--json"],
        &[],
        &[],
    );
    assert_success(&ps2, "ps after up beta");
    let parsed2: Value = serde_json::from_slice(&ps2.stdout).expect("ps json");
    let procs2 = parsed2.get("processes").and_then(Value::as_array).unwrap();
    for p in procs2 {
        let name = p.get("name").and_then(Value::as_str).unwrap_or("?");
        let st = p.get("state").and_then(Value::as_str).unwrap_or("?");
        assert_eq!(st, "running", "service {name} should be running, got {st}");
    }

    let down = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "down", "--json"],
        &[],
        &[],
    );
    assert_success(&down, "down");
}

#[test]
fn start_works_on_unlaunched_config_service() {
    let (_root, project, runtime, state, _config) = setup_project();
    let home = project.parent().expect("parent").join("home");

    let cfg_path = project.join("decompose.yaml");
    fs::write(
        &cfg_path,
        r#"
processes:
  alpha:
    command: "sleep 30"
  beta:
    command: "sleep 30"
"#,
    )
    .expect("write config");
    let cfg = cfg_path.to_string_lossy().to_string();

    // Start only alpha
    let up = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "up", "-d", "--json", "alpha"],
        &[],
        &[],
    );
    assert_success(&up, "up alpha");
    thread::sleep(Duration::from_millis(500));

    // `start beta` should succeed (previously would fail with "unknown service")
    let start = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "start", "--json", "beta"],
        &[],
        &[],
    );
    assert_success(&start, "start beta");
    let start_json: Value = serde_json::from_slice(&start.stdout).expect("start json");
    assert_eq!(
        start_json.get("outcome").and_then(Value::as_str),
        Some("accepted"),
        "start should ack"
    );
    thread::sleep(Duration::from_millis(500));

    // beta should now be running
    let ps = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "ps", "--json"],
        &[],
        &[],
    );
    assert_success(&ps, "ps after start beta");
    let parsed: Value = serde_json::from_slice(&ps.stdout).expect("ps json");
    let procs = parsed.get("processes").and_then(Value::as_array).unwrap();
    let beta = procs
        .iter()
        .find(|p| p.get("name").and_then(Value::as_str) == Some("beta"))
        .expect("beta in ps");
    assert_eq!(
        beta.get("state").and_then(Value::as_str),
        Some("running"),
        "beta should be running after start"
    );

    let down = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "down", "--json"],
        &[],
        &[],
    );
    assert_success(&down, "down");
}

#[test]
fn ps_shows_all_config_services_after_partial_up() {
    let (_root, project, runtime, state, _config) = setup_project();
    let home = project.parent().expect("parent").join("home");

    let cfg_path = project.join("decompose.yaml");
    fs::write(
        &cfg_path,
        r#"
processes:
  alpha:
    command: "sleep 30"
  beta:
    command: "sleep 30"
  gamma:
    command: "sleep 30"
"#,
    )
    .expect("write config");
    let cfg = cfg_path.to_string_lossy().to_string();

    // Start only alpha
    let up = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "up", "-d", "--json", "alpha"],
        &[],
        &[],
    );
    assert_success(&up, "up alpha");
    thread::sleep(Duration::from_millis(500));

    // ps should list all three services
    let ps = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "ps", "--json"],
        &[],
        &[],
    );
    assert_success(&ps, "ps after partial up");
    let parsed: Value = serde_json::from_slice(&ps.stdout).expect("ps json");
    let procs = parsed.get("processes").and_then(Value::as_array).unwrap();
    assert_eq!(
        procs.len(),
        3,
        "should see all 3 config-defined services in ps"
    );

    let names: Vec<&str> = procs
        .iter()
        .filter_map(|p| p.get("name").and_then(Value::as_str))
        .collect();
    assert!(names.contains(&"alpha"), "alpha in ps");
    assert!(names.contains(&"beta"), "beta in ps");
    assert!(names.contains(&"gamma"), "gamma in ps");

    let down = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "down", "--json"],
        &[],
        &[],
    );
    assert_success(&down, "down");
}

#[test]
fn exec_readiness_probe_flips_healthy_flag() {
    let mut env = TestEnv::new();

    // The marker file starts absent; the probe checks for it.
    let marker = env.project.join("healthy_marker");
    let marker_str = marker.to_string_lossy().to_string();

    env.with_config(&format!(
        r#"
processes:
  web:
    command: "sleep 60"
    readiness_probe:
      exec:
        command: "test -f {marker_str}"
      period_seconds: 2
      timeout_seconds: 1
      success_threshold: 1
      failure_threshold: 1
"#
    ));

    env.up_detach_json();

    // Wait for a couple of probe periods — healthy should still be false
    thread::sleep(Duration::from_secs(3));

    let ps1_json = env.ps_json_value();
    let web1 = ps1_json["processes"]
        .as_array()
        .expect("processes array")
        .iter()
        .find(|p| p["name"].as_str() == Some("web"))
        .expect("web process");
    assert_eq!(
        web1["ready"].as_bool(),
        Some(false),
        "ready should be false before marker exists"
    );
    assert_eq!(
        web1["has_readiness_probe"].as_bool(),
        Some(true),
        "has_readiness_probe should be true"
    );

    // Create the marker file so the probe succeeds
    fs::write(&marker, "ok").expect("write marker");

    // Wait for probe to detect it
    thread::sleep(Duration::from_secs(3));

    let ps2_json = env.ps_json_value();
    let web2 = ps2_json["processes"]
        .as_array()
        .expect("processes array")
        .iter()
        .find(|p| p["name"].as_str() == Some("web"))
        .expect("web process");
    assert_eq!(
        web2["ready"].as_bool(),
        Some(true),
        "ready should be true after marker is created"
    );

    env.down_json();
}

#[test]
fn http_get_readiness_probe_flips_healthy_flag() {
    let (_root, project, runtime, state, _config) = setup_project();
    let home = project.parent().expect("parent").join("home");
    let (port, stop_server, server_thread) = spawn_http_ok_server();

    let cfg_path = project.join("decompose.yaml");
    fs::write(
        &cfg_path,
        format!(
            r#"
processes:
  server:
    command: "sleep 30"
    readiness_probe:
      http_get:
        host: "127.0.0.1"
        port: {port}
        path: "/"
      period_seconds: 2
      timeout_seconds: 1
      success_threshold: 1
      failure_threshold: 1
"#
        ),
    )
    .expect("write config");
    let cfg = cfg_path.to_string_lossy().to_string();

    let up = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "up", "--detach", "--json"],
        &[],
        &[],
    );
    assert_success(&up, "up");

    // Poll until the probe flips healthy, with a generous timeout for slow CI.
    let mut healthy = false;
    for _ in 0..30 {
        thread::sleep(Duration::from_secs(1));
        let ps = run_cmd(
            &project,
            &runtime,
            &state,
            &home,
            &["--file", &cfg, "ps", "--json"],
            &[],
            &[],
        );
        if !ps.status.success() {
            continue;
        }
        if let Ok(ps_json) = serde_json::from_slice::<Value>(&ps.stdout)
            && let Some(server) = ps_json["processes"]
                .as_array()
                .and_then(|a| a.iter().find(|p| p["name"].as_str() == Some("server")))
            && server["ready"].as_bool() == Some(true)
        {
            assert_eq!(
                server["has_readiness_probe"].as_bool(),
                Some(true),
                "has_readiness_probe should be true"
            );
            healthy = true;
            break;
        }
    }
    assert!(
        healthy,
        "healthy should be true after HTTP server starts responding (timed out after 30s)"
    );

    let down = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "down", "--json"],
        &[],
        &[],
    );
    assert_success(&down, "down");

    stop_server.store(true, Ordering::Relaxed);
    server_thread.join().expect("test http server thread");
}

#[test]
fn depends_on_process_healthy_gates_dependent_service() {
    let (_root, project, runtime, state, _config) = setup_project();
    let home = project.parent().expect("parent").join("home");

    let marker = project.join("ready_marker");
    let marker_str = marker.to_string_lossy().to_string();

    let cfg_path = project.join("decompose.yaml");
    fs::write(
        &cfg_path,
        format!(
            r#"
processes:
  backend:
    command: "sleep 60"
    readiness_probe:
      exec:
        command: "test -f {marker_str}"
      period_seconds: 2
      timeout_seconds: 1
      success_threshold: 1
      failure_threshold: 1
  frontend:
    command: "sleep 60"
    depends_on:
      backend:
        condition: process_healthy
"#
        ),
    )
    .expect("write config");
    let cfg = cfg_path.to_string_lossy().to_string();

    let up = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "up", "--detach", "--json"],
        &[],
        &[],
    );
    assert_success(&up, "up");

    // Wait a bit — frontend should be pending since backend isn't healthy yet
    thread::sleep(Duration::from_secs(3));

    let ps1 = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "ps", "--json"],
        &[],
        &[],
    );
    assert_success(&ps1, "ps before marker");
    let ps1_json: Value = serde_json::from_slice(&ps1.stdout).expect("ps json");
    let procs1 = ps1_json["processes"].as_array().expect("processes array");
    let frontend1 = procs1
        .iter()
        .find(|p| p["name"].as_str() == Some("frontend"))
        .expect("frontend process");
    assert_eq!(
        frontend1["state"].as_str(),
        Some("pending"),
        "frontend should be pending while backend is unhealthy"
    );

    // Now create the marker to make backend healthy
    fs::write(&marker, "ok").expect("write marker");

    // Wait for probe + supervisor cycle
    thread::sleep(Duration::from_secs(4));

    let ps2 = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "ps", "--json"],
        &[],
        &[],
    );
    assert_success(&ps2, "ps after marker");
    let ps2_json: Value = serde_json::from_slice(&ps2.stdout).expect("ps json");
    let procs2 = ps2_json["processes"].as_array().expect("processes array");
    let frontend2 = procs2
        .iter()
        .find(|p| p["name"].as_str() == Some("frontend"))
        .expect("frontend process");
    assert_eq!(
        frontend2["state"].as_str(),
        Some("running"),
        "frontend should be running after backend becomes healthy"
    );

    let down = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "down", "--json"],
        &[],
        &[],
    );
    assert_success(&down, "down");
}

/// `depends_on: { dep: { condition: process_started } }` — `app` stays
/// `pending` while `dep` is itself gated on an earlier predecessor, and flips
/// to `running` immediately after `dep` reaches `running` (no wait for
/// ready/exit).
///
/// We use a `gate` service that takes ~1s to exit successfully, so `dep` is
/// held in `pending` long enough to observe `app` also parked in `pending`
/// before the chain unlocks.
#[test]
fn depends_on_process_started_gates_dependent_service() {
    let mut env = TestEnv::new();
    env.with_config(
        r#"
processes:
  gate:
    command: "sleep 1 && exit 0"
  dep:
    command: "sleep 30"
    depends_on:
      gate:
        condition: process_completed_successfully
  app:
    command: "sleep 30"
    depends_on:
      dep:
        condition: process_started
"#,
    );

    env.up_detach_json();

    // Early window: gate is still sleeping, so dep hasn't started and app
    // must be pending. Sample a few times during gate's 1s window.
    let mut saw_dep_pending = false;
    let deadline = std::time::Instant::now() + Duration::from_millis(800);
    while std::time::Instant::now() < deadline {
        let parsed = env.ps_json_value();
        let (dep_state, dep_pid) = state_and_pid_of(&parsed, "dep");
        let (app_state, app_pid) = state_and_pid_of(&parsed, "app");
        if dep_state == "pending" {
            assert!(
                dep_pid.is_none(),
                "dep in pending must have no pid, got {dep_pid:?}"
            );
            assert_eq!(
                app_state, "pending",
                "app must be pending while dep is pending, got {app_state:?}"
            );
            assert!(
                app_pid.is_none(),
                "app must not have a pid while dep is pending, got {app_pid:?}"
            );
            saw_dep_pending = true;
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        saw_dep_pending,
        "expected to observe dep in pending state during gate's warm-up window"
    );

    // After gate exits 0, dep starts, and app should follow.
    let (app_state, app_pid) = wait_for_state(&env, "app", Duration::from_secs(15), |s, p| {
        s == "running" && p.is_some()
    });
    assert_eq!(
        app_state, "running",
        "app should reach running once dep starts, got {app_state:?}"
    );
    assert!(app_pid.is_some(), "app must have a pid once running");

    let parsed = env.ps_json_value();
    let (dep_final_state, dep_final_pid) = state_and_pid_of(&parsed, "dep");
    assert_eq!(dep_final_state, "running");
    assert!(dep_final_pid.is_some());
}

/// `depends_on: { dep: { condition: process_completed } }` — `app` must stay
/// `pending` while `dep` is running, then launch once `dep` terminates
/// regardless of exit code. Uses a nonzero exit to confirm the `_successfully`
/// variant is what filters on code.
#[test]
fn depends_on_process_completed_gates_dependent_service() {
    let mut env = TestEnv::new();
    env.with_config(
        r#"
processes:
  dep:
    command: "sleep 2 && exit 3"
  app:
    command: "sleep 30"
    depends_on:
      dep:
        condition: process_completed
"#,
    );

    env.up_detach_json();

    // Sample quickly: dep is mid-sleep (running), app must still be pending.
    thread::sleep(Duration::from_millis(400));
    let parsed = env.ps_json_value();
    let (dep_early, _) = state_and_pid_of(&parsed, "dep");
    let (app_early, app_early_pid) = state_and_pid_of(&parsed, "app");
    assert_eq!(
        dep_early, "running",
        "dep should be mid-sleep when first sampled, got {dep_early:?}"
    );
    assert_eq!(
        app_early, "pending",
        "app must be pending while dep is running, got {app_early:?}"
    );
    assert!(
        app_early_pid.is_none(),
        "app must not have a pid before dep completes, got {app_early_pid:?}"
    );

    // Wait for dep to exit and app to launch.
    let (app_state, app_pid) = wait_for_state(&env, "app", Duration::from_secs(10), |s, p| {
        s == "running" && p.is_some()
    });
    assert_eq!(
        app_state, "running",
        "app should launch after dep exits (any code), got {app_state:?}"
    );
    assert!(app_pid.is_some());

    // Sanity-check dep: it exited nonzero, so `ps` reports it as "failed"
    // (per ProcessStatus::state_label for Exited with non-zero code).
    let parsed = env.ps_json_value();
    let (dep_final, _) = state_and_pid_of(&parsed, "dep");
    assert_eq!(
        dep_final, "failed",
        "dep exited with code 3 → surfaced as failed, got {dep_final:?}"
    );
}

/// `depends_on: { dep: { condition: process_completed_successfully } }` —
/// positive path (dep exits 0 → app starts) and negative path (dep exits 1
/// → app stays `pending` forever).
#[test]
fn depends_on_process_completed_successfully_positive_and_negative() {
    // Positive: dep exits 0, app must start.
    {
        let mut env = TestEnv::new();
        env.with_config(
            r#"
processes:
  dep:
    command: "sleep 1 && exit 0"
  app:
    command: "sleep 30"
    depends_on:
      dep:
        condition: process_completed_successfully
"#,
        );

        env.up_detach_json();

        let (app_state, app_pid) = wait_for_state(&env, "app", Duration::from_secs(10), |s, p| {
            s == "running" && p.is_some()
        });
        assert_eq!(
            app_state, "running",
            "app should start once dep exits 0, got {app_state:?}"
        );
        assert!(app_pid.is_some());

        let parsed = env.ps_json_value();
        let (dep_state, _) = state_and_pid_of(&parsed, "dep");
        assert_eq!(dep_state, "exited", "dep should surface as exited (code 0)");
    }

    // Negative: dep exits 1, app stays pending indefinitely — `_successfully`
    // never satisfies on a nonzero exit.
    {
        let mut env = TestEnv::new();
        env.with_config(
            r#"
processes:
  dep:
    command: "sleep 1 && exit 1"
  app:
    command: "sleep 30"
    depends_on:
      dep:
        condition: process_completed_successfully
"#,
        );

        env.up_detach_json();

        // Wait long enough for dep to exit, plus a few supervisor ticks.
        thread::sleep(Duration::from_secs(3));

        let parsed = env.ps_json_value();
        let (dep_state, _) = state_and_pid_of(&parsed, "dep");
        let (app_state, app_pid) = state_and_pid_of(&parsed, "app");
        assert_eq!(
            dep_state, "failed",
            "dep exited 1 → failed label, got {dep_state:?}"
        );
        assert_eq!(
            app_state, "pending",
            "app must stay pending when dep fails under \
             process_completed_successfully, got {app_state:?}"
        );
        assert!(
            app_pid.is_none(),
            "app must not launch on failed dep, got pid={app_pid:?}"
        );
    }
}

/// `depends_on: { dep: { condition: process_log_ready } }` — `app` stays
/// `pending` until `dep` emits a line matching `ready_log_line`, then
/// launches. The dep writes a non-matching line first, sleeps, then emits the
/// ready token.
#[test]
fn depends_on_process_log_ready_gates_dependent_service() {
    let mut env = TestEnv::new();
    env.with_config(
        r#"
processes:
  dep:
    command: "echo booting; sleep 2; echo SERVER_READY; sleep 30"
    ready_log_line: "SERVER_READY"
  app:
    command: "sleep 30"
    depends_on:
      dep:
        condition: process_log_ready
"#,
    );

    env.up_detach_json();

    // Early window: dep is running but hasn't emitted the ready token yet.
    thread::sleep(Duration::from_millis(400));
    let parsed = env.ps_json_value();
    let (dep_early, dep_early_pid) = state_and_pid_of(&parsed, "dep");
    let (app_early, app_early_pid) = state_and_pid_of(&parsed, "app");
    assert_eq!(
        dep_early, "running",
        "dep should be running in warm-up window, got {dep_early:?}"
    );
    assert!(
        dep_early_pid.is_some(),
        "dep must have a pid, got {dep_early_pid:?}"
    );
    assert_eq!(
        app_early, "pending",
        "app must be pending before dep logs ready token, got {app_early:?}"
    );
    assert!(
        app_early_pid.is_none(),
        "app must not have a pid before ready log, got {app_early_pid:?}"
    );

    // After the echo fires, app should transition to running.
    let (app_state, app_pid) = wait_for_state(&env, "app", Duration::from_secs(10), |s, p| {
        s == "running" && p.is_some()
    });
    assert_eq!(
        app_state, "running",
        "app should launch once dep emits SERVER_READY, got {app_state:?}"
    );
    assert!(app_pid.is_some());
}

#[test]
fn liveness_probe_kills_process_on_failure() {
    let (_root, project, runtime, state, _config) = setup_project();
    let home = project.parent().expect("parent").join("home");

    // The liveness probe always fails (test -f on a file that never exists).
    // With restart_policy: on_failure and failure_threshold: 2, the liveness
    // probe should kill the process after 2 consecutive failures, causing a
    // restart.
    let cfg_path = project.join("decompose.yaml");
    fs::write(
        &cfg_path,
        r#"
processes:
  victim:
    command: "sleep 120"
    restart_policy: on_failure
    backoff_seconds: 1
    liveness_probe:
      exec:
        command: "false"
      period_seconds: 2
      timeout_seconds: 1
      failure_threshold: 2
      initial_delay_seconds: 1
"#,
    )
    .expect("write config");
    let cfg = cfg_path.to_string_lossy().to_string();

    let up = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "up", "--detach", "--json"],
        &[],
        &[],
    );
    assert_success(&up, "up");

    // Wait for initial_delay (1s) + 2 probe failures (2s) + restart backoff (1s) + buffer
    thread::sleep(Duration::from_secs(7));

    let ps = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "ps", "--json"],
        &[],
        &[],
    );
    assert_success(&ps, "ps");
    let ps_json: Value = serde_json::from_slice(&ps.stdout).expect("ps json");
    let victim = ps_json["processes"]
        .as_array()
        .expect("processes array")
        .iter()
        .find(|p| p["name"].as_str() == Some("victim"))
        .expect("victim process");
    let restart_count = victim["restart_count"].as_u64().unwrap_or(0);
    assert!(
        restart_count >= 1,
        "liveness probe should have killed the process causing a restart, got restart_count={restart_count}"
    );

    let down = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "down", "--json"],
        &[],
        &[],
    );
    assert_success(&down, "down");
}

#[test]
fn readiness_and_liveness_probes_track_independent_flags() {
    // Regression test for the ready/alive flag split. A service with both
    // readiness and liveness probes must report each flag independently in
    // ps JSON. The readiness probe passes (marker file present), so
    // `ready=true`; the liveness probe fails (`false`), so after the
    // failure_threshold the daemon marks `alive=false` and SIGKILLs the
    // process — causing `restart_count` to tick up via on_failure policy.
    let (_root, project, runtime, state, _config) = setup_project();
    let home = project.parent().expect("parent").join("home");

    let marker = project.join("ready_marker");
    fs::write(&marker, "ok").expect("write marker");
    let marker_str = marker.to_string_lossy().to_string();

    let cfg_path = project.join("decompose.yaml");
    fs::write(
        &cfg_path,
        format!(
            r#"
processes:
  svc:
    command: "sleep 120"
    restart_policy: on_failure
    backoff_seconds: 1
    readiness_probe:
      exec:
        command: "test -f {marker_str}"
      period_seconds: 1
      timeout_seconds: 1
      success_threshold: 1
      failure_threshold: 1
      initial_delay_seconds: 0
    liveness_probe:
      exec:
        command: "false"
      period_seconds: 1
      timeout_seconds: 1
      failure_threshold: 2
      initial_delay_seconds: 1
"#
        ),
    )
    .expect("write config");
    let cfg = cfg_path.to_string_lossy().to_string();

    let up = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "up", "--detach", "--json"],
        &[],
        &[],
    );
    assert_success(&up, "up");

    // Poll until we observe ready=true && alive=false simultaneously. The
    // readiness probe flips `ready` on the first tick (~1s); the liveness
    // probe waits the initial_delay (1s) + 2 failures at 1s each (~3s) and
    // then kills the process — which resets `alive` to true on the next
    // spawn. So we must catch it in that narrow window, or rely on seeing
    // restart_count > 0 as evidence the liveness path fired.
    let mut saw_ready_and_not_alive = false;
    let mut saw_restart = false;
    let mut last_snapshot = Value::Null;
    for _ in 0..20 {
        thread::sleep(Duration::from_millis(500));
        let ps = run_cmd(
            &project,
            &runtime,
            &state,
            &home,
            &["--file", &cfg, "ps", "--json"],
            &[],
            &[],
        );
        if !ps.status.success() {
            continue;
        }
        let Ok(ps_json) = serde_json::from_slice::<Value>(&ps.stdout) else {
            continue;
        };
        let Some(svc) = ps_json["processes"]
            .as_array()
            .and_then(|a| a.iter().find(|p| p["base"].as_str() == Some("svc")))
        else {
            continue;
        };

        last_snapshot = svc.clone();

        // Additive JSON fields must be present and typed correctly.
        assert!(
            svc.get("ready").and_then(Value::as_bool).is_some(),
            "ProcessSnapshot must expose bool 'ready', got: {svc}"
        );
        assert!(
            svc.get("alive").and_then(Value::as_bool).is_some(),
            "ProcessSnapshot must expose bool 'alive', got: {svc}"
        );
        assert_eq!(
            svc["has_liveness_probe"].as_bool(),
            Some(true),
            "has_liveness_probe must be exposed and true"
        );

        if svc["ready"].as_bool() == Some(true) && svc["alive"].as_bool() == Some(false) {
            saw_ready_and_not_alive = true;
        }
        if svc["restart_count"].as_u64().unwrap_or(0) >= 1 {
            saw_restart = true;
        }
        if saw_ready_and_not_alive && saw_restart {
            break;
        }
    }

    assert!(
        saw_ready_and_not_alive,
        "expected to observe ready=true && alive=false in some ps snapshot — flags stomp each other"
    );
    assert!(
        saw_restart,
        "liveness probe failure must still trigger a restart after the flag split: {last_snapshot}"
    );

    let down = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "down", "--json"],
        &[],
        &[],
    );
    assert_success(&down, "down");
}

#[test]
fn healthy_resets_on_process_restart() {
    let (_root, project, runtime, state, _config) = setup_project();
    let home = project.parent().expect("parent").join("home");

    let marker = project.join("health_marker");
    let marker_str = marker.to_string_lossy().to_string();

    // Long-running process with a readiness probe. We create a marker so the
    // probe succeeds, verify healthy=true, then remove the marker and trigger
    // a restart via `decompose restart`. After restart, healthy should reset
    // to false and stay false since the marker is gone.
    let cfg_path = project.join("decompose.yaml");
    fs::write(
        &cfg_path,
        format!(
            r#"
processes:
  svc:
    command: "sleep 60"
    readiness_probe:
      exec:
        command: "test -f {marker_str}"
      period_seconds: 2
      timeout_seconds: 1
      success_threshold: 1
      failure_threshold: 1
"#
        ),
    )
    .expect("write config");
    let cfg = cfg_path.to_string_lossy().to_string();

    // Create marker so probe succeeds immediately
    fs::write(&marker, "ok").expect("write marker");

    let up = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "up", "--detach", "--json"],
        &[],
        &[],
    );
    assert_success(&up, "up");

    // Wait for probe to detect marker
    thread::sleep(Duration::from_secs(3));

    let ps1 = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "ps", "--json"],
        &[],
        &[],
    );
    assert_success(&ps1, "ps before restart");
    let ps1_json: Value = serde_json::from_slice(&ps1.stdout).expect("ps json");
    let svc1 = ps1_json["processes"]
        .as_array()
        .expect("processes array")
        .iter()
        .find(|p| p["name"].as_str() == Some("svc"))
        .expect("svc process");
    assert_eq!(
        svc1["ready"].as_bool(),
        Some(true),
        "ready should be true before restart"
    );

    // Remove marker and trigger a restart
    fs::remove_file(&marker).expect("remove marker");

    let restart = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "restart", "svc", "--json"],
        &[],
        &[],
    );
    assert_success(&restart, "restart");

    // Wait for stop + re-spawn + probe to fail
    thread::sleep(Duration::from_secs(4));

    let ps2 = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "ps", "--json"],
        &[],
        &[],
    );
    assert_success(&ps2, "ps after restart without marker");
    let ps2_json: Value = serde_json::from_slice(&ps2.stdout).expect("ps json");
    let svc2 = ps2_json["processes"]
        .as_array()
        .expect("processes array")
        .iter()
        .find(|p| p["name"].as_str() == Some("svc"))
        .expect("svc process");
    assert_eq!(
        svc2["ready"].as_bool(),
        Some(false),
        "ready should be false after restart when marker is gone"
    );

    let down = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "down", "--json"],
        &[],
        &[],
    );
    assert_success(&down, "down");
}

#[test]
fn up_creates_directories_and_files_with_restrictive_perms() {
    use std::os::unix::fs::PermissionsExt;

    let (_root, project, runtime, state, config) = setup_project();
    let home = project.parent().expect("parent").join("home");
    let cfg = config.to_string_lossy().to_string();

    let up = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "up", "--detach", "--json"],
        &[],
        &[],
    );
    assert_success(&up, "up");

    // Give the daemon a moment to write its log file.
    thread::sleep(Duration::from_millis(500));

    let runtime_decompose = runtime.join("decompose");
    let state_decompose = state.join("decompose");

    let rt_mode = fs::metadata(&runtime_decompose)
        .expect("runtime/decompose exists")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(
        rt_mode, 0o700,
        "runtime dir should be 0o700, got {rt_mode:o}"
    );

    let st_mode = fs::metadata(&state_decompose)
        .expect("state/decompose exists")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(st_mode, 0o700, "state dir should be 0o700, got {st_mode:o}");

    // Locate the instance-specific files by scanning for extensions.
    let mut log_file = None;
    let mut pid_file = None;
    let mut lock_file = None;
    for entry in fs::read_dir(&state_decompose).expect("read state dir") {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        match path.extension().and_then(|s| s.to_str()) {
            Some("log") => log_file = Some(path),
            Some("pid") => pid_file = Some(path),
            Some("lock") => lock_file = Some(path),
            _ => {}
        }
    }

    let log_path = log_file.expect("daemon log file");
    let pid_path = pid_file.expect("pid file");
    let lock_path = lock_file.expect("lock file");

    for (label, p) in [("log", &log_path), ("pid", &pid_path), ("lock", &lock_path)] {
        let mode = fs::metadata(p)
            .unwrap_or_else(|e| panic!("{label} file stat: {e}"))
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "{label} should be 0o600, got {mode:o}");
    }

    // Find the socket in the runtime dir and verify its perms.
    let mut socket_file = None;
    for entry in fs::read_dir(&runtime_decompose).expect("read runtime dir") {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("sock") {
            socket_file = Some(path);
        }
    }
    let sock_path = socket_file.expect("socket file");
    let sock_mode = fs::metadata(&sock_path)
        .expect("socket stat")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(
        sock_mode, 0o600,
        "socket should be 0o600, got {sock_mode:o}"
    );

    let down = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "down", "--json"],
        &[],
        &[],
    );
    assert_success(&down, "down");
}

// ---------------------------------------------------------------------------
// Config-reload integration tests
//
// These exercise the Reload IPC + reconcile loop via the `up` CLI entry
// point: when `up` runs against a live daemon it sends `Reload` before
// `Start`, and the `--force-recreate` / `--no-recreate` / `--remove-orphans`
// / `--no-start` flags are plumbed through to the daemon's plan executor.
// ---------------------------------------------------------------------------

/// Small helper used across the reload tests to rewrite the config file
/// in-place. Kept local to this section because the semantics are
/// "overwrite whatever was there" - simpler than a builder.
fn rewrite_config(cfg_path: &Path, contents: &str) {
    fs::write(cfg_path, contents).expect("rewrite config");
}

/// Extract the running pid of a named process from `ps --json` output.
/// Returns `None` when the process is absent or has no pid (e.g. not_started).
fn pid_of(ps_json: &Value, name: &str) -> Option<u64> {
    ps_json
        .get("processes")
        .and_then(Value::as_array)?
        .iter()
        .find(|p| p.get("name").and_then(Value::as_str) == Some(name))
        .and_then(|p| p.get("pid").and_then(Value::as_u64))
}

fn state_of(ps_json: &Value, name: &str) -> Option<String> {
    ps_json
        .get("processes")
        .and_then(Value::as_array)?
        .iter()
        .find(|p| p.get("name").and_then(Value::as_str) == Some(name))
        .and_then(|p| p.get("state").and_then(Value::as_str))
        .map(std::string::ToString::to_string)
}

#[test]
fn reload_adds_new_service() {
    let (_root, project, runtime, state, _config) = setup_project();
    let home = project.parent().expect("parent").join("home");
    let cfg_path = project.join("decompose.yaml");
    rewrite_config(
        &cfg_path,
        r#"
processes:
  alpha:
    command: "sleep 30"
"#,
    );
    let cfg = cfg_path.to_string_lossy().to_string();

    let up1 = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "up", "-d", "--json"],
        &[],
        &[],
    );
    assert_success(&up1, "first up");
    thread::sleep(Duration::from_millis(300));

    let ps1 = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "ps", "--json"],
        &[],
        &[],
    );
    assert_success(&ps1, "ps after first up");
    let parsed1: Value = serde_json::from_slice(&ps1.stdout).expect("ps json");
    let procs1 = parsed1.get("processes").and_then(Value::as_array).unwrap();
    assert_eq!(procs1.len(), 1, "only alpha should be present");

    // Rewrite config to add beta, then re-run up.
    rewrite_config(
        &cfg_path,
        r#"
processes:
  alpha:
    command: "sleep 30"
  beta:
    command: "sleep 30"
"#,
    );

    let up2 = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "up", "-d", "--json"],
        &[],
        &[],
    );
    assert_success(&up2, "second up after adding beta");
    thread::sleep(Duration::from_millis(500));

    let ps2 = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "ps", "--json"],
        &[],
        &[],
    );
    assert_success(&ps2, "ps after second up");
    let parsed2: Value = serde_json::from_slice(&ps2.stdout).expect("ps json");
    let procs2 = parsed2.get("processes").and_then(Value::as_array).unwrap();
    assert_eq!(procs2.len(), 2, "alpha + beta after reload");
    assert_eq!(state_of(&parsed2, "alpha").as_deref(), Some("running"));
    assert_eq!(state_of(&parsed2, "beta").as_deref(), Some("running"));

    let down = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "down", "--json"],
        &[],
        &[],
    );
    assert_success(&down, "down");
}

#[test]
fn reload_removes_service_leaves_orphan_by_default() {
    let (_root, project, runtime, state, _config) = setup_project();
    let home = project.parent().expect("parent").join("home");
    let cfg_path = project.join("decompose.yaml");
    rewrite_config(
        &cfg_path,
        r#"
processes:
  alpha:
    command: "sleep 30"
  beta:
    command: "sleep 30"
"#,
    );
    let cfg = cfg_path.to_string_lossy().to_string();

    let up1 = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "up", "-d", "--json"],
        &[],
        &[],
    );
    assert_success(&up1, "first up");
    thread::sleep(Duration::from_millis(300));

    // Remove beta from the config, re-run up without --remove-orphans.
    rewrite_config(
        &cfg_path,
        r#"
processes:
  alpha:
    command: "sleep 30"
"#,
    );

    let up2 = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "up", "-d", "--json"],
        &[],
        &[],
    );
    assert_success(&up2, "second up without --remove-orphans");
    // The Ack's reload message is printed on stdout. It carries the word
    // "orphan" when services were removed from config without cleanup.
    let stdout = String::from_utf8_lossy(&up2.stdout);
    assert!(
        stdout.contains("orphan"),
        "reload ack should mention 'orphan', got: {stdout}"
    );

    thread::sleep(Duration::from_millis(300));

    let ps = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "ps", "--json"],
        &[],
        &[],
    );
    assert_success(&ps, "ps after reload");
    let parsed: Value = serde_json::from_slice(&ps.stdout).expect("ps json");
    assert_eq!(state_of(&parsed, "alpha").as_deref(), Some("running"));
    // beta is left running as an orphan even though it's no longer in config.
    assert_eq!(
        state_of(&parsed, "beta").as_deref(),
        Some("running"),
        "orphan beta should still be running by default"
    );

    let down = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "down", "--json"],
        &[],
        &[],
    );
    assert_success(&down, "down");
}

#[test]
fn reload_with_remove_orphans_stops_removed_service() {
    let (_root, project, runtime, state, _config) = setup_project();
    let home = project.parent().expect("parent").join("home");
    let cfg_path = project.join("decompose.yaml");
    rewrite_config(
        &cfg_path,
        r#"
processes:
  alpha:
    command: "sleep 30"
  beta:
    command: "sleep 30"
"#,
    );
    let cfg = cfg_path.to_string_lossy().to_string();

    let up1 = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "up", "-d", "--json"],
        &[],
        &[],
    );
    assert_success(&up1, "first up");
    thread::sleep(Duration::from_millis(300));

    rewrite_config(
        &cfg_path,
        r#"
processes:
  alpha:
    command: "sleep 30"
"#,
    );

    let up2 = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "up", "-d", "--json", "--remove-orphans"],
        &[],
        &[],
    );
    assert_success(&up2, "second up with --remove-orphans");
    thread::sleep(Duration::from_millis(500));

    let ps = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "ps", "--json"],
        &[],
        &[],
    );
    assert_success(&ps, "ps after remove-orphans reload");
    let parsed: Value = serde_json::from_slice(&ps.stdout).expect("ps json");
    let procs = parsed.get("processes").and_then(Value::as_array).unwrap();
    assert_eq!(
        procs.len(),
        1,
        "only alpha should remain after --remove-orphans, got: {parsed}"
    );
    assert_eq!(state_of(&parsed, "alpha").as_deref(), Some("running"));

    let down = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "down", "--json"],
        &[],
        &[],
    );
    assert_success(&down, "down");
}

#[test]
fn reload_modified_command_recreates_service() {
    let (_root, project, runtime, state, _config) = setup_project();
    let home = project.parent().expect("parent").join("home");
    let cfg_path = project.join("decompose.yaml");
    rewrite_config(
        &cfg_path,
        r#"
processes:
  alpha:
    command: "sleep 30"
"#,
    );
    let cfg = cfg_path.to_string_lossy().to_string();

    let up1 = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "up", "-d", "--json"],
        &[],
        &[],
    );
    assert_success(&up1, "first up");
    thread::sleep(Duration::from_millis(300));

    let ps_before = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "ps", "--json"],
        &[],
        &[],
    );
    assert_success(&ps_before, "ps before reload");
    let parsed_before: Value = serde_json::from_slice(&ps_before.stdout).expect("ps json");
    let pid_before = pid_of(&parsed_before, "alpha").expect("alpha pid before");

    // Change alpha's command, forcing a hash divergence.
    rewrite_config(
        &cfg_path,
        r#"
processes:
  alpha:
    command: "sleep 60"
"#,
    );

    let up2 = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "up", "-d", "--json"],
        &[],
        &[],
    );
    assert_success(&up2, "second up with modified command");
    thread::sleep(Duration::from_millis(800));

    let ps_after = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "ps", "--json"],
        &[],
        &[],
    );
    assert_success(&ps_after, "ps after reload");
    let parsed_after: Value = serde_json::from_slice(&ps_after.stdout).expect("ps json");
    let pid_after = pid_of(&parsed_after, "alpha").expect("alpha pid after");
    assert_eq!(
        state_of(&parsed_after, "alpha").as_deref(),
        Some("running"),
        "alpha should be running after recreate"
    );
    assert_ne!(
        pid_before, pid_after,
        "changed command should spawn a new pid (before={pid_before}, after={pid_after})"
    );

    let down = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "down", "--json"],
        &[],
        &[],
    );
    assert_success(&down, "down");
}

#[test]
fn reload_unchanged_service_not_restarted() {
    let (_root, project, runtime, state, _config) = setup_project();
    let home = project.parent().expect("parent").join("home");
    let cfg_path = project.join("decompose.yaml");
    rewrite_config(
        &cfg_path,
        r#"
processes:
  alpha:
    command: "sleep 30"
"#,
    );
    let cfg = cfg_path.to_string_lossy().to_string();

    let up1 = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "up", "-d", "--json"],
        &[],
        &[],
    );
    assert_success(&up1, "first up");
    thread::sleep(Duration::from_millis(300));

    let ps_before = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "ps", "--json"],
        &[],
        &[],
    );
    assert_success(&ps_before, "ps before reload");
    let parsed_before: Value = serde_json::from_slice(&ps_before.stdout).expect("ps json");
    let pid_before = pid_of(&parsed_before, "alpha").expect("alpha pid before");

    // Add an unrelated service; alpha's hash is unchanged.
    rewrite_config(
        &cfg_path,
        r#"
processes:
  alpha:
    command: "sleep 30"
  beta:
    command: "sleep 30"
"#,
    );

    let up2 = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "up", "-d", "--json"],
        &[],
        &[],
    );
    assert_success(&up2, "second up adds beta, alpha unchanged");
    thread::sleep(Duration::from_millis(500));

    let ps_after = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "ps", "--json"],
        &[],
        &[],
    );
    assert_success(&ps_after, "ps after reload");
    let parsed_after: Value = serde_json::from_slice(&ps_after.stdout).expect("ps json");
    let pid_after = pid_of(&parsed_after, "alpha").expect("alpha pid after");
    assert_eq!(
        pid_before, pid_after,
        "unchanged alpha should keep its pid across reload"
    );
    assert_eq!(
        state_of(&parsed_after, "beta").as_deref(),
        Some("running"),
        "newly-added beta should be running"
    );

    let down = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "down", "--json"],
        &[],
        &[],
    );
    assert_success(&down, "down");
}

#[test]
fn reload_force_recreate_recreates_unchanged_service() {
    let (_root, project, runtime, state, _config) = setup_project();
    let home = project.parent().expect("parent").join("home");
    let cfg_path = project.join("decompose.yaml");
    rewrite_config(
        &cfg_path,
        r#"
processes:
  alpha:
    command: "sleep 30"
"#,
    );
    let cfg = cfg_path.to_string_lossy().to_string();

    let up1 = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "up", "-d", "--json"],
        &[],
        &[],
    );
    assert_success(&up1, "first up");
    thread::sleep(Duration::from_millis(300));

    let ps_before = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "ps", "--json"],
        &[],
        &[],
    );
    assert_success(&ps_before, "ps before reload");
    let parsed_before: Value = serde_json::from_slice(&ps_before.stdout).expect("ps json");
    let pid_before = pid_of(&parsed_before, "alpha").expect("alpha pid before");

    // No config change, but --force-recreate forces a respawn.
    let up2 = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "up", "-d", "--json", "--force-recreate"],
        &[],
        &[],
    );
    assert_success(&up2, "second up --force-recreate");
    thread::sleep(Duration::from_millis(800));

    let ps_after = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "ps", "--json"],
        &[],
        &[],
    );
    assert_success(&ps_after, "ps after --force-recreate");
    let parsed_after: Value = serde_json::from_slice(&ps_after.stdout).expect("ps json");
    let pid_after = pid_of(&parsed_after, "alpha").expect("alpha pid after");
    assert_eq!(
        state_of(&parsed_after, "alpha").as_deref(),
        Some("running"),
        "alpha should be running after --force-recreate"
    );
    assert_ne!(
        pid_before, pid_after,
        "--force-recreate should respawn alpha (before={pid_before}, after={pid_after})"
    );

    let down = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "down", "--json"],
        &[],
        &[],
    );
    assert_success(&down, "down");
}

#[test]
fn reload_no_recreate_preserves_changed_service() {
    let (_root, project, runtime, state, _config) = setup_project();
    let home = project.parent().expect("parent").join("home");
    let cfg_path = project.join("decompose.yaml");
    rewrite_config(
        &cfg_path,
        r#"
processes:
  alpha:
    command: "sleep 30"
"#,
    );
    let cfg = cfg_path.to_string_lossy().to_string();

    let up1 = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "up", "-d", "--json"],
        &[],
        &[],
    );
    assert_success(&up1, "first up");
    thread::sleep(Duration::from_millis(300));

    let ps_before = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "ps", "--json"],
        &[],
        &[],
    );
    assert_success(&ps_before, "ps before reload");
    let parsed_before: Value = serde_json::from_slice(&ps_before.stdout).expect("ps json");
    let pid_before = pid_of(&parsed_before, "alpha").expect("alpha pid before");

    // Change the command, but pass --no-recreate so the running instance stays.
    rewrite_config(
        &cfg_path,
        r#"
processes:
  alpha:
    command: "sleep 60"
"#,
    );

    let up2 = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "up", "-d", "--json", "--no-recreate"],
        &[],
        &[],
    );
    assert_success(&up2, "second up --no-recreate");
    thread::sleep(Duration::from_millis(500));

    let ps_after = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "ps", "--json"],
        &[],
        &[],
    );
    assert_success(&ps_after, "ps after --no-recreate");
    let parsed_after: Value = serde_json::from_slice(&ps_after.stdout).expect("ps json");
    let pid_after = pid_of(&parsed_after, "alpha").expect("alpha pid after");
    assert_eq!(
        pid_before, pid_after,
        "--no-recreate should keep the hash-diverged alpha alive (before={pid_before}, after={pid_after})"
    );

    let down = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "down", "--json"],
        &[],
        &[],
    );
    assert_success(&down, "down");
}

#[test]
fn reload_no_start_registers_service_without_starting_it() {
    let (_root, project, runtime, state, _config) = setup_project();
    let home = project.parent().expect("parent").join("home");
    let cfg_path = project.join("decompose.yaml");
    rewrite_config(
        &cfg_path,
        r#"
processes:
  alpha:
    command: "sleep 30"
"#,
    );
    let cfg = cfg_path.to_string_lossy().to_string();

    let up1 = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "up", "-d", "--json"],
        &[],
        &[],
    );
    assert_success(&up1, "first up");
    thread::sleep(Duration::from_millis(300));

    // Add beta and run `up --no-start` so beta is registered but parked.
    rewrite_config(
        &cfg_path,
        r#"
processes:
  alpha:
    command: "sleep 30"
  beta:
    command: "sleep 30"
"#,
    );

    let up2 = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "up", "-d", "--json", "--no-start"],
        &[],
        &[],
    );
    assert_success(&up2, "second up --no-start");
    thread::sleep(Duration::from_millis(300));

    let ps = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "ps", "--json"],
        &[],
        &[],
    );
    assert_success(&ps, "ps after --no-start");
    let parsed: Value = serde_json::from_slice(&ps.stdout).expect("ps json");
    let beta_state = state_of(&parsed, "beta").unwrap_or_default();
    assert_ne!(
        beta_state, "running",
        "beta should NOT be running after --no-start, got: {beta_state}"
    );
    // Concretely, the daemon parks --no-start entries in NotStarted.
    assert_eq!(
        beta_state, "not_started",
        "beta should be parked as not_started, got: {beta_state}"
    );

    // Follow-up `start` should bring beta up.
    let start = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "start", "--json", "beta"],
        &[],
        &[],
    );
    assert_success(&start, "start beta");
    thread::sleep(Duration::from_millis(500));

    let ps2 = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "ps", "--json"],
        &[],
        &[],
    );
    assert_success(&ps2, "ps after start beta");
    let parsed2: Value = serde_json::from_slice(&ps2.stdout).expect("ps json");
    assert_eq!(
        state_of(&parsed2, "beta").as_deref(),
        Some("running"),
        "beta should be running after explicit start"
    );

    let down = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "down", "--json"],
        &[],
        &[],
    );
    assert_success(&down, "down");
}

#[test]
fn reload_parse_error_does_not_affect_running_services() {
    let (_root, project, runtime, state, _config) = setup_project();
    let home = project.parent().expect("parent").join("home");
    let cfg_path = project.join("decompose.yaml");
    rewrite_config(
        &cfg_path,
        r#"
processes:
  alpha:
    command: "sleep 30"
"#,
    );
    let cfg = cfg_path.to_string_lossy().to_string();

    let up1 = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "up", "-d", "--json"],
        &[],
        &[],
    );
    assert_success(&up1, "first up");
    thread::sleep(Duration::from_millis(300));

    let ps_before = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "ps", "--json"],
        &[],
        &[],
    );
    assert_success(&ps_before, "ps before invalid rewrite");
    let parsed_before: Value = serde_json::from_slice(&ps_before.stdout).expect("ps json");
    let pid_before = pid_of(&parsed_before, "alpha").expect("alpha pid before");

    // Rewrite the config to invalid YAML.
    rewrite_config(&cfg_path, "not: valid: yaml: [[[");

    let up_bad = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "up", "-d", "--json"],
        &[],
        &[],
    );
    assert!(
        !up_bad.status.success(),
        "up with invalid yaml should fail; stdout={}, stderr={}",
        String::from_utf8_lossy(&up_bad.stdout),
        String::from_utf8_lossy(&up_bad.stderr)
    );

    // Restore a valid config so ps (which also resolves config) works, and
    // confirm alpha is still running with the same pid.
    rewrite_config(
        &cfg_path,
        r#"
processes:
  alpha:
    command: "sleep 30"
"#,
    );

    let ps_after = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "ps", "--json"],
        &[],
        &[],
    );
    assert_success(&ps_after, "ps after failed reload");
    let parsed_after: Value = serde_json::from_slice(&ps_after.stdout).expect("ps json");
    let pid_after = pid_of(&parsed_after, "alpha").expect("alpha pid after");
    assert_eq!(
        pid_before, pid_after,
        "alpha pid must be untouched after a failed reload (before={pid_before}, after={pid_after})"
    );

    let down = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "down", "--json"],
        &[],
        &[],
    );
    assert_success(&down, "down");
}

#[test]
fn reload_rejects_removed_service_still_depended_on() {
    let (_root, project, runtime, state, _config) = setup_project();
    let home = project.parent().expect("parent").join("home");
    let cfg_path = project.join("decompose.yaml");
    rewrite_config(
        &cfg_path,
        r#"
processes:
  alpha:
    command: "sleep 30"
  beta:
    command: "sleep 30"
    depends_on:
      alpha:
        condition: process_started
"#,
    );
    let cfg = cfg_path.to_string_lossy().to_string();

    let up1 = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "up", "-d", "--json"],
        &[],
        &[],
    );
    assert_success(&up1, "first up");
    thread::sleep(Duration::from_millis(500));

    let ps_before = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "ps", "--json"],
        &[],
        &[],
    );
    assert_success(&ps_before, "ps before bad reload");
    let parsed_before: Value = serde_json::from_slice(&ps_before.stdout).expect("ps json");
    let alpha_before = pid_of(&parsed_before, "alpha").expect("alpha pid before");
    let beta_before = pid_of(&parsed_before, "beta").expect("beta pid before");

    // Remove alpha but keep beta - beta still declares a dep on alpha.
    rewrite_config(
        &cfg_path,
        r#"
processes:
  beta:
    command: "sleep 30"
    depends_on:
      alpha:
        condition: process_started
"#,
    );

    let up_bad = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "up", "-d", "--json"],
        &[],
        &[],
    );
    assert!(
        !up_bad.status.success(),
        "up with dep-violation should fail; stdout={}, stderr={}",
        String::from_utf8_lossy(&up_bad.stdout),
        String::from_utf8_lossy(&up_bad.stderr)
    );
    let stderr = String::from_utf8_lossy(&up_bad.stderr);
    assert!(
        stderr.contains("depends on") || stderr.contains("removed"),
        "error should mention the dep violation, got: {stderr}"
    );

    // Fix the config before ps / down, and confirm both services still
    // running with their original pids.
    rewrite_config(
        &cfg_path,
        r#"
processes:
  alpha:
    command: "sleep 30"
  beta:
    command: "sleep 30"
    depends_on:
      alpha:
        condition: process_started
"#,
    );

    let ps_after = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "ps", "--json"],
        &[],
        &[],
    );
    assert_success(&ps_after, "ps after rejected reload");
    let parsed_after: Value = serde_json::from_slice(&ps_after.stdout).expect("ps json");
    let alpha_after = pid_of(&parsed_after, "alpha").expect("alpha pid after");
    let beta_after = pid_of(&parsed_after, "beta").expect("beta pid after");
    assert_eq!(
        alpha_before, alpha_after,
        "alpha pid must be untouched by a rejected reload"
    );
    assert_eq!(
        beta_before, beta_after,
        "beta pid must be untouched by a rejected reload"
    );

    let down = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "down", "--json"],
        &[],
        &[],
    );
    assert_success(&down, "down");
}

#[test]
fn reload_scale_up_preserves_existing_replica_pids() {
    // Scale 2 → 3. The existing foo[1], foo[2] must keep their pids; only
    // foo[3] is newly spawned. Using 2→3 rather than 1→2 avoids the
    // naming boundary (single replica is named `foo`, not `foo[1]`); that
    // transition falls back to full recreate by design.
    let (_root, project, runtime, state, _config) = setup_project();
    let home = project.parent().expect("parent").join("home");
    let cfg_path = project.join("decompose.yaml");
    rewrite_config(
        &cfg_path,
        r#"
processes:
  foo:
    command: "sleep 30"
    replicas: 2
"#,
    );
    let cfg = cfg_path.to_string_lossy().to_string();

    let up1 = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "up", "-d", "--json"],
        &[],
        &[],
    );
    assert_success(&up1, "first up (replicas=2)");
    thread::sleep(Duration::from_millis(400));

    let ps_before = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "ps", "--json"],
        &[],
        &[],
    );
    assert_success(&ps_before, "ps before scale-up");
    let parsed_before: Value = serde_json::from_slice(&ps_before.stdout).expect("ps json");
    let pid1_before = pid_of(&parsed_before, "foo[1]").expect("foo[1] pid before");
    let pid2_before = pid_of(&parsed_before, "foo[2]").expect("foo[2] pid before");

    // Scale up to 3.
    rewrite_config(
        &cfg_path,
        r#"
processes:
  foo:
    command: "sleep 30"
    replicas: 3
"#,
    );

    let up2 = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "up", "-d", "--json"],
        &[],
        &[],
    );
    assert_success(&up2, "second up (replicas=3)");
    let stdout2 = String::from_utf8_lossy(&up2.stdout);
    assert!(
        stdout2.contains("scaled"),
        "reload ack should mention 'scaled', got: {stdout2}"
    );
    thread::sleep(Duration::from_millis(600));

    let ps_after = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "ps", "--json"],
        &[],
        &[],
    );
    assert_success(&ps_after, "ps after scale-up");
    let parsed_after: Value = serde_json::from_slice(&ps_after.stdout).expect("ps json");
    let pid1_after = pid_of(&parsed_after, "foo[1]").expect("foo[1] pid after");
    let pid2_after = pid_of(&parsed_after, "foo[2]").expect("foo[2] pid after");
    let pid3_after = pid_of(&parsed_after, "foo[3]").expect("foo[3] pid after");
    assert_eq!(pid1_before, pid1_after, "foo[1] pid must be preserved");
    assert_eq!(pid2_before, pid2_after, "foo[2] pid must be preserved");
    assert!(pid3_after > 0, "foo[3] should be running with a valid pid");

    let down = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "down", "--json"],
        &[],
        &[],
    );
    assert_success(&down, "down");
}

#[test]
fn reload_scale_down_stops_highest_indexed_replica() {
    // Scale 3 → 2. foo[1] and foo[2] keep their pids; foo[3] goes away.
    let (_root, project, runtime, state, _config) = setup_project();
    let home = project.parent().expect("parent").join("home");
    let cfg_path = project.join("decompose.yaml");
    rewrite_config(
        &cfg_path,
        r#"
processes:
  foo:
    command: "sleep 30"
    replicas: 3
"#,
    );
    let cfg = cfg_path.to_string_lossy().to_string();

    let up1 = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "up", "-d", "--json"],
        &[],
        &[],
    );
    assert_success(&up1, "first up (replicas=3)");
    thread::sleep(Duration::from_millis(500));

    let ps_before = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "ps", "--json"],
        &[],
        &[],
    );
    assert_success(&ps_before, "ps before scale-down");
    let parsed_before: Value = serde_json::from_slice(&ps_before.stdout).expect("ps json");
    let pid1_before = pid_of(&parsed_before, "foo[1]").expect("foo[1] pid before");
    let pid2_before = pid_of(&parsed_before, "foo[2]").expect("foo[2] pid before");
    let pid3_before = pid_of(&parsed_before, "foo[3]").expect("foo[3] pid before");
    assert!(pid3_before > 0);

    // Scale down to 2.
    rewrite_config(
        &cfg_path,
        r#"
processes:
  foo:
    command: "sleep 30"
    replicas: 2
"#,
    );

    let up2 = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "up", "-d", "--json"],
        &[],
        &[],
    );
    assert_success(&up2, "second up (replicas=2)");
    let stdout2 = String::from_utf8_lossy(&up2.stdout);
    assert!(
        stdout2.contains("scaled"),
        "reload ack should mention 'scaled', got: {stdout2}"
    );
    thread::sleep(Duration::from_millis(800));

    let ps_after = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "ps", "--json"],
        &[],
        &[],
    );
    assert_success(&ps_after, "ps after scale-down");
    let parsed_after: Value = serde_json::from_slice(&ps_after.stdout).expect("ps json");
    let procs = parsed_after
        .get("processes")
        .and_then(Value::as_array)
        .expect("processes array");
    assert_eq!(procs.len(), 2, "only foo[1] and foo[2] should remain");
    let pid1_after = pid_of(&parsed_after, "foo[1]").expect("foo[1] pid after");
    let pid2_after = pid_of(&parsed_after, "foo[2]").expect("foo[2] pid after");
    assert_eq!(pid1_before, pid1_after, "foo[1] pid must be preserved");
    assert_eq!(pid2_before, pid2_after, "foo[2] pid must be preserved");
    assert!(
        pid_of(&parsed_after, "foo[3]").is_none(),
        "foo[3] must be gone after scale-down"
    );

    let down = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "down", "--json"],
        &[],
        &[],
    );
    assert_success(&down, "down");
}

#[test]
fn reload_scale_one_to_n_renames_existing_instance() {
    // Scale 1 → 2. The existing single-replica instance is named `foo`
    // (unqualified); when replicas >= 2 every replica is named `foo[N]`.
    // The daemon must rename the surviving instance in place (`foo` →
    // `foo[1]`) so its pid is preserved across the boundary crossing.
    // `foo[2]` is newly spawned.
    let (_root, project, runtime, state, _config) = setup_project();
    let home = project.parent().expect("parent").join("home");
    let cfg_path = project.join("decompose.yaml");
    rewrite_config(
        &cfg_path,
        r#"
processes:
  foo:
    command: "sleep 30"
"#,
    );
    let cfg = cfg_path.to_string_lossy().to_string();

    let up1 = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "up", "-d", "--json"],
        &[],
        &[],
    );
    assert_success(&up1, "first up (replicas=1)");
    thread::sleep(Duration::from_millis(400));

    let ps_before = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "ps", "--json"],
        &[],
        &[],
    );
    assert_success(&ps_before, "ps before scale-up");
    let parsed_before: Value = serde_json::from_slice(&ps_before.stdout).expect("ps json");
    let pid_before = pid_of(&parsed_before, "foo").expect("foo pid before");
    assert!(pid_before > 0, "foo must be running before scale-up");

    rewrite_config(
        &cfg_path,
        r#"
processes:
  foo:
    command: "sleep 30"
    replicas: 2
"#,
    );

    let up2 = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "up", "-d", "--json"],
        &[],
        &[],
    );
    assert_success(&up2, "second up (replicas=2)");
    let stdout2 = String::from_utf8_lossy(&up2.stdout);
    assert!(
        stdout2.contains("scaled"),
        "reload ack should report a scaled transition (not a full recreate), got: {stdout2}"
    );
    assert!(
        stdout2.contains("renamed"),
        "reload ack should mention 'renamed' for the 1↔N boundary crossing, got: {stdout2}"
    );
    thread::sleep(Duration::from_millis(600));

    let ps_after = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "ps", "--json"],
        &[],
        &[],
    );
    assert_success(&ps_after, "ps after scale-up");
    let parsed_after: Value = serde_json::from_slice(&ps_after.stdout).expect("ps json");
    let pid1_after = pid_of(&parsed_after, "foo[1]").expect("foo[1] pid after");
    let pid2_after = pid_of(&parsed_after, "foo[2]").expect("foo[2] pid after");
    assert_eq!(
        pid_before, pid1_after,
        "the original `foo` pid must be preserved as `foo[1]` after scale-up"
    );
    assert!(
        pid2_after > 0 && pid2_after != pid_before,
        "foo[2] must be a freshly-spawned process"
    );
    // Sanity: the unqualified `foo` entry should no longer appear in ps.
    assert!(
        pid_of(&parsed_after, "foo").is_none(),
        "unqualified `foo` must be gone after rename"
    );

    let down = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "down", "--json"],
        &[],
        &[],
    );
    assert_success(&down, "down");
}

#[test]
fn reload_scale_n_to_one_renames_surviving_instance() {
    // Scale 2 → 1. `foo[2]` is stopped; the surviving `foo[1]` is renamed
    // to the unqualified `foo` in place. The pid must persist.
    let (_root, project, runtime, state, _config) = setup_project();
    let home = project.parent().expect("parent").join("home");
    let cfg_path = project.join("decompose.yaml");
    rewrite_config(
        &cfg_path,
        r#"
processes:
  foo:
    command: "sleep 30"
    replicas: 2
"#,
    );
    let cfg = cfg_path.to_string_lossy().to_string();

    let up1 = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "up", "-d", "--json"],
        &[],
        &[],
    );
    assert_success(&up1, "first up (replicas=2)");
    thread::sleep(Duration::from_millis(500));

    let ps_before = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "ps", "--json"],
        &[],
        &[],
    );
    assert_success(&ps_before, "ps before scale-down");
    let parsed_before: Value = serde_json::from_slice(&ps_before.stdout).expect("ps json");
    let pid1_before = pid_of(&parsed_before, "foo[1]").expect("foo[1] pid before");
    let pid2_before = pid_of(&parsed_before, "foo[2]").expect("foo[2] pid before");
    assert!(pid1_before > 0 && pid2_before > 0);

    rewrite_config(
        &cfg_path,
        r#"
processes:
  foo:
    command: "sleep 30"
"#,
    );

    let up2 = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "up", "-d", "--json"],
        &[],
        &[],
    );
    assert_success(&up2, "second up (replicas=1)");
    let stdout2 = String::from_utf8_lossy(&up2.stdout);
    assert!(
        stdout2.contains("scaled"),
        "reload ack should report a scaled transition, got: {stdout2}"
    );
    assert!(
        stdout2.contains("renamed"),
        "reload ack should mention 'renamed' for the 1↔N boundary crossing, got: {stdout2}"
    );
    thread::sleep(Duration::from_millis(800));

    let ps_after = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "ps", "--json"],
        &[],
        &[],
    );
    assert_success(&ps_after, "ps after scale-down");
    let parsed_after: Value = serde_json::from_slice(&ps_after.stdout).expect("ps json");
    let procs = parsed_after
        .get("processes")
        .and_then(Value::as_array)
        .expect("processes array");
    assert_eq!(
        procs.len(),
        1,
        "only the single renamed `foo` should remain"
    );
    let pid_after = pid_of(&parsed_after, "foo").expect("foo pid after");
    assert_eq!(
        pid1_before, pid_after,
        "the surviving `foo[1]` pid must be preserved as `foo` after scale-down"
    );
    assert!(
        pid_of(&parsed_after, "foo[1]").is_none(),
        "`foo[1]` must be gone after rename"
    );
    assert!(
        pid_of(&parsed_after, "foo[2]").is_none(),
        "`foo[2]` must be gone after scale-down"
    );

    let down = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "down", "--json"],
        &[],
        &[],
    );
    assert_success(&down, "down");
}

#[test]
fn immediate_exit_process_reaches_exited_state() {
    // Covers the edge case where a process exits before the supervisor has
    // any chance to transition it past Pending/Running — the bookkeeping
    // must still catch the exit and report `exited`/`failed` rather than
    // leaving a zombie "running" row. `true` on PATH returns instantly on
    // every POSIX system we target.
    let (_root, project, runtime, state, _config) = setup_project();
    let home = project.parent().expect("parent").join("home");

    let cfg_path = project.join("decompose.yaml");
    fs::write(
        &cfg_path,
        r#"
processes:
  quick_ok:
    command: "true"
  quick_fail:
    command: "sh -c 'exit 7'"
"#,
    )
    .expect("write config");
    let cfg = cfg_path.to_string_lossy().to_string();

    let up = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "up", "--detach", "--json"],
        &[],
        &[],
    );
    assert_success(&up, "up with immediate-exit processes");

    // Poll briefly: by the time `up --detach` returns the daemon is up,
    // but the supervisor tick may not have observed the exit yet.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let (mut ok_state, mut fail_state, mut fail_code) = (String::new(), String::new(), None);
    while std::time::Instant::now() < deadline {
        let ps = run_cmd(
            &project,
            &runtime,
            &state,
            &home,
            &["--file", &cfg, "ps", "--json"],
            &[],
            &[],
        );
        assert_success(&ps, "ps");
        let ps_json: Value = serde_json::from_slice(&ps.stdout).expect("ps json");
        let processes = ps_json
            .get("processes")
            .and_then(Value::as_array)
            .expect("processes array");
        ok_state = processes
            .iter()
            .find(|p| p.get("name").and_then(Value::as_str) == Some("quick_ok"))
            .and_then(|p| p.get("state").and_then(Value::as_str))
            .unwrap_or("")
            .to_string();
        let fail_proc = processes
            .iter()
            .find(|p| p.get("name").and_then(Value::as_str) == Some("quick_fail"));
        fail_state = fail_proc
            .and_then(|p| p.get("state").and_then(Value::as_str))
            .unwrap_or("")
            .to_string();
        fail_code = fail_proc.and_then(|p| p.get("exit_code").and_then(Value::as_i64));
        if ok_state == "exited" && (fail_state == "failed" || fail_state == "exited") {
            break;
        }
        thread::sleep(Duration::from_millis(150));
    }

    assert_eq!(
        ok_state, "exited",
        "quick_ok should reach terminal `exited` state"
    );
    assert!(
        fail_state == "failed" || fail_state == "exited",
        "quick_fail should reach a terminal state, got: {fail_state}"
    );
    assert_eq!(fail_code, Some(7), "quick_fail exit_code captured");

    let down = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "down", "--json"],
        &[],
        &[],
    );
    assert_success(&down, "down");
}

#[test]
fn concurrent_up_invocations_coexist() {
    // Two `decompose up --detach` processes start simultaneously against
    // the same project dir. The daemon's flock() and the CLI's
    // Ping-then-spawn race guard should let both invocations return
    // success — one spawns the daemon, the other reconnects to it. Neither
    // may leave the daemon in a broken state.
    let (_root, project, runtime, state, _config) = setup_project();
    let home = project.parent().expect("parent").join("home");

    let cfg_path = project.join("decompose.yaml");
    fs::write(
        &cfg_path,
        r#"
processes:
  sleeper:
    command: "sleep 30"
"#,
    )
    .expect("write config");
    let cfg = cfg_path.to_string_lossy().to_string();

    let spawn_up = || {
        let mut cmd = Command::new(bin_path());
        cmd.current_dir(&project)
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("XDG_STATE_HOME", &state)
            .env("HOME", &home)
            .args(["--file", &cfg, "up", "--detach", "--json"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd.spawn().expect("spawn up")
    };

    let child_a = spawn_up();
    let child_b = spawn_up();

    let out_a = child_a.wait_with_output().expect("wait a");
    let out_b = child_b.wait_with_output().expect("wait b");

    assert_success(&out_a, "concurrent up A");
    assert_success(&out_b, "concurrent up B");

    // Both responses should agree on the daemon pid — there's only one.
    // `up --detach --json` may emit a progress line followed by the final
    // result JSON when it's the invocation that spawns the daemon, so parse
    // the last complete JSON value from stdout rather than expecting one.
    let parse_last_json = |stdout: &[u8], label: &str| -> Value {
        let text = std::str::from_utf8(stdout).expect("utf8");
        text.lines()
            .rev()
            .find_map(|line| {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    serde_json::from_str::<Value>(trimmed).ok()
                }
            })
            .unwrap_or_else(|| panic!("{label}: no JSON object in stdout: {text}"))
    };
    let a_json = parse_last_json(&out_a.stdout, "a json");
    let b_json = parse_last_json(&out_b.stdout, "b json");
    let pid_a = a_json["daemon"].get("pid").and_then(Value::as_u64);
    let pid_b = b_json["daemon"].get("pid").and_then(Value::as_u64);
    assert!(pid_a.is_some(), "a must report a daemon pid");
    assert_eq!(pid_a, pid_b, "both invocations must see the same daemon");

    let ps = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "ps", "--json"],
        &[],
        &[],
    );
    assert_success(&ps, "ps after concurrent up");
    let ps_json: Value = serde_json::from_slice(&ps.stdout).expect("ps json");
    let processes = ps_json
        .get("processes")
        .and_then(Value::as_array)
        .expect("processes array");
    assert_eq!(processes.len(), 1, "only one sleeper instance");
    let state_str = processes[0]
        .get("state")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert_eq!(state_str, "running", "sleeper should be running");

    let down = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "down", "--json"],
        &[],
        &[],
    );
    assert_success(&down, "down");
}

#[test]
fn shutdown_terminates_grandchild_processes() {
    // A shell command that forks off a long-lived grandchild. On `down`
    // the daemon signals the whole process group so the grandchild dies
    // with its parent.
    let (_root, project, runtime, state, _config) = setup_project();
    let home = project.parent().expect("parent").join("home");

    let pidfile = project.join("child.pid");
    let readyfile = project.join("child.ready");
    let cfg_path = project.join("decompose.yaml");
    // Parent prints the grandchild's pid to a file, writes a ready marker,
    // then waits. The grandchild is `sleep 60` so it outlives the test
    // unless we actually signal the whole group.
    let shell = format!(
        "sh -c 'sleep 60 & echo $! > {pid}; touch {ready}; wait'",
        pid = pidfile.display(),
        ready = readyfile.display()
    );
    fs::write(
        &cfg_path,
        format!(
            r#"
processes:
  forker:
    command: {shell:?}
"#
        ),
    )
    .expect("write config");
    let cfg = cfg_path.to_string_lossy().to_string();

    let up = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "up", "--detach", "--json"],
        &[],
        &[],
    );
    assert_success(&up, "up forker");

    // Wait until the parent has forked and the pidfile + ready marker
    // exist — this is a deterministic signal, not a timing guess.
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while !readyfile.exists() && std::time::Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        readyfile.exists(),
        "child did not record its pid within the deadline"
    );

    let child_pid: i32 = fs::read_to_string(&pidfile)
        .expect("read pid")
        .trim()
        .parse()
        .expect("parse pid");

    assert!(
        process_alive(child_pid as u32),
        "grandchild should be alive before down"
    );

    let down = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "down", "--json"],
        &[],
        &[],
    );
    assert_success(&down, "down forker");

    // Give the kernel a moment to reap. Poll rather than sleep blindly.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut still_alive = true;
    while std::time::Instant::now() < deadline {
        if !process_alive(child_pid as u32) {
            still_alive = false;
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    if still_alive {
        // Best effort cleanup so the grandchild doesn't outlive the test
        // binary even when we fail.
        let _ = kill(Pid::from_raw(child_pid), Signal::SIGKILL);
        panic!("grandchild pid {child_pid} survived `down` — process group was not signalled");
    }
}

#[test]
fn logs_no_pager_writes_directly_to_stdout() {
    // Integration test note: the test harness captures the child's stdout
    // via a pipe, so stdout is *not* a TTY here and paging wouldn't engage
    // anyway. We still exercise --no-pager explicitly to confirm:
    //   1. The flag parses and the command exits cleanly.
    //   2. Log content reaches stdout directly (not via pager).
    //   3. `DECOMPOSE_PAGER` set to something that would fail loudly (e.g.
    //      `false`) does NOT run when --no-pager wins the gate.
    let (_root, project, runtime, state, _config) = setup_project();
    let home = project.parent().expect("parent").join("home");

    let cfg_path = project.join("decompose.yaml");
    fs::write(
        &cfg_path,
        r#"
processes:
  talker:
    command: "sh -c 'echo HELLO_FROM_TALKER; sleep 30'"
"#,
    )
    .expect("write config");
    let cfg = cfg_path.to_string_lossy().to_string();

    let up = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "up", "--detach", "--json"],
        &[],
        &[],
    );
    assert_success(&up, "up talker");

    // Wait for the log line to appear on disk before asking for logs.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut saw_line = false;
    while std::time::Instant::now() < deadline {
        let logs = run_cmd(
            &project,
            &runtime,
            &state,
            &home,
            &["--file", &cfg, "logs", "--no-pager"],
            // Set DECOMPOSE_PAGER to `false` (always exit 1). If --no-pager
            // were ignored and we *did* spawn this, the pager process would
            // exit 1 before any output got written to our stdout. So a
            // successful exit + the log content on stdout proves the bypass.
            &[("DECOMPOSE_PAGER", "false")],
            &[],
        );
        assert_success(&logs, "logs --no-pager");
        let text = String::from_utf8_lossy(&logs.stdout);
        if text.contains("HELLO_FROM_TALKER") {
            saw_line = true;
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    assert!(
        saw_line,
        "expected HELLO_FROM_TALKER in logs --no-pager output"
    );

    // Also sanity-check the flag is recognized in --help output so we notice
    // if a rename or removal happens later.
    let help = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["logs", "--help"],
        &[],
        &[],
    );
    assert_success(&help, "logs --help");
    let help_text = String::from_utf8_lossy(&help.stdout);
    assert!(
        help_text.contains("--no-pager"),
        "logs --help should document --no-pager, got:\n{help_text}"
    );

    let down = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "down", "--json"],
        &[],
        &[],
    );
    assert_success(&down, "down talker");
}

/// `logs -f` must print the existing backlog before streaming new output,
/// matching `docker compose logs -f` / `tail -f` semantics (decompose-sn7).
/// Regression: previously it tailed from EOF, so any lines emitted before the
/// command ran were invisible.
#[test]
fn logs_follow_prints_backlog_then_streams_new_lines() {
    use std::io::{BufRead, BufReader};
    use std::sync::{Arc, Mutex};

    let (_root, project, runtime, state, _config) = setup_project();
    let home = project.parent().expect("parent").join("home");

    // A service that emits a distinctive backlog line, pauses to let us see
    // that line hit disk, then emits ticks so we can prove the follower is
    // still streaming after draining the backlog.
    let cfg_path = project.join("decompose.yaml");
    fs::write(
        &cfg_path,
        r#"
processes:
  ticker:
    command: "sh -c 'echo BACKLOG_LINE_Z9; sleep 1; i=0; while :; do i=$((i+1)); echo TICK_$i; sleep 1; done'"
"#,
    )
    .expect("write config");
    let cfg = cfg_path.to_string_lossy().to_string();

    let up = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "up", "--detach", "--json"],
        &[],
        &[],
    );
    assert_success(&up, "up ticker");

    // Wait for BACKLOG_LINE_Z9 to land on disk via a non-follow `logs` read
    // — that guarantees it truly is "backlog" from the follower's viewpoint.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut backlog_on_disk = false;
    while std::time::Instant::now() < deadline {
        let out = run_cmd(
            &project,
            &runtime,
            &state,
            &home,
            &["--file", &cfg, "logs", "--no-pager"],
            &[],
            &[],
        );
        assert_success(&out, "logs --no-pager");
        if String::from_utf8_lossy(&out.stdout).contains("BACKLOG_LINE_Z9") {
            backlog_on_disk = true;
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    assert!(backlog_on_disk, "backlog line never appeared on disk");

    // Spawn `decompose logs -f` as a child and capture its stdout line-by-line
    // on a background thread into a shared buffer.
    let mut follower = Command::new(bin_path())
        .current_dir(&project)
        .env("XDG_RUNTIME_DIR", &runtime)
        .env("XDG_STATE_HOME", &state)
        .env("HOME", &home)
        .args(["--file", &cfg, "logs", "-f"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn logs -f");

    let stdout = follower.stdout.take().expect("follower stdout");
    let collected: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let collected_reader = Arc::clone(&collected);
    let reader_thread = thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines().map_while(Result::ok) {
            collected_reader.lock().expect("lock").push(line);
        }
    });

    // First assertion: backlog line appears in follower output.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut saw_backlog = false;
    while std::time::Instant::now() < deadline {
        if collected
            .lock()
            .expect("lock")
            .iter()
            .any(|l| l.contains("BACKLOG_LINE_Z9"))
        {
            saw_backlog = true;
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    // Second assertion: a TICK_N emitted *after* the follower started shows up
    // — proves streaming continues past the backlog replay.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut saw_tick = false;
    while std::time::Instant::now() < deadline {
        if collected
            .lock()
            .expect("lock")
            .iter()
            .any(|l| l.contains("TICK_"))
        {
            saw_tick = true;
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }

    let _ = follower.kill();
    let _ = follower.wait();
    let _ = reader_thread.join();

    let snapshot = collected.lock().expect("lock").clone();

    assert!(
        saw_backlog,
        "expected BACKLOG_LINE_Z9 in follower stdout (backlog replay).\nlines:\n{}",
        snapshot.join("\n")
    );
    assert!(
        saw_tick,
        "expected a TICK_N line in follower stdout (live streaming).\nlines:\n{}",
        snapshot.join("\n")
    );

    let down = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "down", "--json"],
        &[],
        &[],
    );
    assert_success(&down, "down ticker");
}

// ---------------------------------------------------------------------------
// `run` and `exec` (decompose-s2g)
// ---------------------------------------------------------------------------

/// `run` works when no daemon is running — it should read the config
/// directly, spawn the command with the service's env/cwd, and exit with the
/// child's code. No IPC needed.
#[test]
fn run_works_without_daemon() {
    let (_root, project, runtime, state, _cfg) = setup_project();
    let home = project.parent().expect("parent").join("home");
    // Overwrite config with a service that has a distinctive env var we can
    // echo back from the one-off command.
    let cfg_path = project.join("decompose.yaml");
    fs::write(
        &cfg_path,
        r#"
processes:
  worker:
    command: "sleep 30"
    environment:
      DECOMPOSE_TEST_VAR: hello-from-service
"#,
    )
    .expect("write config");
    let cfg = cfg_path.to_string_lossy().to_string();

    let output = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &[
            "--file",
            &cfg,
            "run",
            "worker",
            "sh",
            "-c",
            "printf '%s' \"$DECOMPOSE_TEST_VAR\"",
        ],
        &[],
        &[],
    );
    assert_success(&output, "run worker");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        stdout.trim(),
        "hello-from-service",
        "run should inherit service env, got: {stdout}"
    );
}

/// `run` propagates the child's non-zero exit code.
#[test]
fn run_propagates_exit_code() {
    let (_root, project, runtime, state, cfg) = setup_project();
    let home = project.parent().expect("parent").join("home");
    let cfg_str = cfg.to_string_lossy().to_string();

    let output = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg_str, "run", "sleeper", "sh", "-c", "exit 42"],
        &[],
        &[],
    );
    assert_eq!(
        output.status.code(),
        Some(42),
        "expected exit 42, got {:?}",
        output.status.code()
    );
}

/// `run` fails clearly when the service doesn't exist.
#[test]
fn run_rejects_unknown_service() {
    let (_root, project, runtime, state, cfg) = setup_project();
    let home = project.parent().expect("parent").join("home");
    let cfg_str = cfg.to_string_lossy().to_string();

    let output = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg_str, "run", "does-not-exist", "echo", "hi"],
        &[],
        &[],
    );
    assert!(!output.status.success(), "run unknown-service should fail");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unknown service"),
        "stderr should mention unknown service, got: {stderr}"
    );
}

/// `exec` refuses to run when no daemon is running, pointing the user at `up`
/// or `run`.
#[test]
fn exec_fails_without_daemon() {
    let (_root, project, runtime, state, cfg) = setup_project();
    let home = project.parent().expect("parent").join("home");
    let cfg_str = cfg.to_string_lossy().to_string();

    let output = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg_str, "exec", "sleeper", "echo", "hi"],
        &[],
        &[],
    );
    assert!(!output.status.success(), "exec should fail without daemon");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no running environment"),
        "stderr should explain no daemon running, got: {stderr}"
    );
}

/// `exec` refuses to run when the service is defined but no replica is
/// currently Running (e.g. stopped or not yet started).
#[test]
fn exec_fails_when_service_not_running() {
    let (_root, project, runtime, state, _cfg) = setup_project();
    let home = project.parent().expect("parent").join("home");
    // Two services: `alive` is running; `dead` is disabled so it never runs.
    let cfg_path = project.join("decompose.yaml");
    fs::write(
        &cfg_path,
        r#"
processes:
  alive:
    command: "sleep 30"
  dead:
    command: "sleep 30"
    disabled: true
"#,
    )
    .expect("write config");
    let cfg = cfg_path.to_string_lossy().to_string();

    let up = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "up", "--detach", "--json"],
        &[],
        &[],
    );
    assert_success(&up, "up");
    // Give `alive` a moment to reach Running.
    thread::sleep(Duration::from_millis(500));

    let output = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "exec", "dead", "echo", "hi"],
        &[],
        &[],
    );
    assert!(
        !output.status.success(),
        "exec on disabled service should fail"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("not running"),
        "stderr should explain service not running, got: {stderr}"
    );

    let down = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "down"],
        &[],
        &[],
    );
    assert_success(&down, "down");
}

/// `exec` succeeds when the service has a running replica — it spawns the
/// user command with the service's environment and returns the child's exit
/// code.
#[test]
fn exec_runs_when_service_is_running() {
    let (_root, project, runtime, state, _cfg) = setup_project();
    let home = project.parent().expect("parent").join("home");
    let cfg_path = project.join("decompose.yaml");
    fs::write(
        &cfg_path,
        r#"
processes:
  db:
    command: "sleep 30"
    environment:
      DB_URL: "postgres://localhost/test"
"#,
    )
    .expect("write config");
    let cfg = cfg_path.to_string_lossy().to_string();

    let up = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "up", "--detach", "--json"],
        &[],
        &[],
    );
    assert_success(&up, "up db");
    thread::sleep(Duration::from_millis(500));

    let output = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &[
            "--file",
            &cfg,
            "exec",
            "db",
            "sh",
            "-c",
            "printf '%s' \"$DB_URL\"",
        ],
        &[],
        &[],
    );
    assert_success(&output, "exec db");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout.trim(), "postgres://localhost/test");

    // `-e` overrides take precedence.
    let output2 = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &[
            "--file",
            &cfg,
            "exec",
            "--env",
            "DB_URL=postgres://override/db",
            "db",
            "sh",
            "-c",
            "printf '%s' \"$DB_URL\"",
        ],
        &[],
        &[],
    );
    assert_success(&output2, "exec -e override");
    let stdout2 = String::from_utf8_lossy(&output2.stdout);
    assert_eq!(stdout2.trim(), "postgres://override/db");

    let down = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "down"],
        &[],
        &[],
    );
    assert_success(&down, "down");
}

/// `--workdir`/`-w` overrides the service's working directory.
#[test]
fn run_workdir_override() {
    let (root, project, runtime, state, cfg) = setup_project();
    let home = project.parent().expect("parent").join("home");
    let cfg_str = cfg.to_string_lossy().to_string();

    let alt_dir = root.path().join("altwd");
    fs::create_dir_all(&alt_dir).expect("create altwd");

    let output = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &[
            "--file",
            &cfg_str,
            "run",
            "-w",
            alt_dir.to_str().unwrap(),
            "sleeper",
            "sh",
            "-c",
            "pwd",
        ],
        &[],
        &[],
    );
    assert_success(&output, "run with -w");
    let stdout = String::from_utf8_lossy(&output.stdout);
    // On macOS `pwd` may report `/private/var/...` vs `/var/...`; accept either
    // the original path or a path that ends with the same suffix.
    let trimmed = stdout.trim();
    let alt_str = alt_dir.to_string_lossy();
    assert!(
        trimmed == alt_str || trimmed.ends_with(alt_str.trim_start_matches('/')),
        "pwd should be {alt_str}, got {trimmed}"
    );
}

/// Check whether the daemon is currently responsive via a `ps` IPC
/// round-trip. The CLI's `ps` returns exit 0 either way — `{"running":
/// false, "processes": []}` when no daemon answers, a plain
/// `{"processes":[...]}` when one does — so we distinguish by payload.
fn is_daemon_live_ipc(
    project: &Path,
    runtime: &Path,
    state: &Path,
    home: &Path,
    cfg: &str,
) -> bool {
    let out = run_cmd(
        project,
        runtime,
        state,
        home,
        &["--file", cfg, "ps", "--json"],
        &[],
        &[],
    );
    if !out.status.success() {
        return false;
    }
    let parsed: Value = match serde_json::from_slice(&out.stdout) {
        Ok(v) => v,
        Err(_) => return false,
    };
    parsed["daemon"]["state"] == "running"
}

/// Observe daemon liveness using its PID file and native process state,
/// independently of whether the IPC server is responsive.
/// Returns `true` if the PID file exists and the referenced process is
/// running.
fn is_daemon_live_no_ipc(state: &Path) -> bool {
    // We don't know the instance hash here, so scan the state dir for any
    // `*.pid` file the daemon may have written under this test's
    // XDG_STATE_HOME.
    let state_dir = state.join("decompose");
    let Ok(entries) = fs::read_dir(&state_dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("pid") {
            continue;
        }
        let Ok(contents) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(pid) = contents.trim().parse::<i32>() else {
            continue;
        };
        if process_alive(pid as u32) {
            return true;
        }
    }
    false
}

/// Poll the IPC probe until the daemon becomes responsive or the deadline
/// expires. Returns whether the daemon was reachable in time.
fn wait_for_daemon_up_ipc(
    project: &Path,
    runtime: &Path,
    state: &Path,
    home: &Path,
    cfg: &str,
    deadline: Duration,
) -> bool {
    let start = std::time::Instant::now();
    loop {
        if is_daemon_live_ipc(project, runtime, state, home, cfg) {
            return true;
        }
        if start.elapsed() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(200));
    }
}

/// Poll the non-IPC liveness probe (PID file + `kill -0`) until the
/// daemon exits or the deadline expires. Does NOT issue IPC requests, so
/// it won't falsely bump the orphan-watchdog clock.
fn wait_for_daemon_exit_no_ipc(state: &Path, deadline: Duration) -> bool {
    let start = std::time::Instant::now();
    loop {
        if !is_daemon_live_no_ipc(state) {
            return true;
        }
        if start.elapsed() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(200));
    }
}

#[test]
fn detached_up_daemon_survives_without_parent_pid() {
    // `up -d` should not set up orphan-watchdog — the daemon is meant to
    // outlive the launching process. We verify that even with an
    // aggressively-short DECOMPOSE_ORPHAN_TIMEOUT, the daemon sticks around
    // after the `up` invocation that started it has already exited.
    let (_root, project, runtime, state, config) = setup_project();
    let home = project.parent().expect("parent").join("home");
    let cfg = config.to_string_lossy().to_string();

    let up = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "up", "--detach", "--json"],
        &[("DECOMPOSE_ORPHAN_TIMEOUT", "2")],
        &[],
    );
    assert_success(&up, "up --detach");

    // Wait well past the grace period. A misconfigured detached daemon
    // would auto-exit here.
    thread::sleep(Duration::from_secs(5));

    assert!(
        is_daemon_live_no_ipc(&state),
        "detached daemon must survive after orphan-timeout window (parent_pid should be unset)",
    );

    let down = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "down", "--json"],
        &[],
        &[],
    );
    assert_success(&down, "down detached");
}

#[test]
fn attached_up_killed_triggers_daemon_auto_exit() {
    // Attached `up` (no --detach) launches the daemon with --parent-pid.
    // If we SIGKILL the `up` parent (so it can't call down), the daemon
    // should observe the orphaned state and self-exit after the grace
    // period elapses with no IPC activity.
    let (_root, project, runtime, state, config) = setup_project();
    let home = project.parent().expect("parent").join("home");
    let cfg = config.to_string_lossy().to_string();

    let mut up = Command::new(bin_path());
    up.current_dir(&project)
        .env("XDG_RUNTIME_DIR", &runtime)
        .env("XDG_STATE_HOME", &state)
        .env("HOME", &home)
        .env("DECOMPOSE_ORPHAN_TIMEOUT", "2")
        .arg("--file")
        .arg(&cfg)
        .arg("up")
        .arg("--table")
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    let mut child = up.spawn().expect("spawn attached up");

    // Give the daemon a chance to come up.
    assert!(
        wait_for_daemon_up_ipc(
            &project,
            &runtime,
            &state,
            &home,
            &cfg,
            Duration::from_secs(10),
        ),
        "daemon never became responsive",
    );

    // SIGKILL the attached `up` so it can't call down. The `up` process is
    // the declared parent-pid; once it's gone, no further IPC requests
    // are needed: the watchdog should stop the environment independently
    // of client activity.
    kill(Pid::from_raw(child.id() as i32), Signal::SIGKILL).expect("send sigkill");
    let _ = child.wait();

    // Grace is 2s, watchdog tick is 1s. Allow generous slack.
    let exited = wait_for_daemon_exit_no_ipc(&state, Duration::from_secs(15));
    assert!(
        exited,
        "daemon should self-exit after orphan grace period elapsed",
    );
}

#[test]
fn client_activity_does_not_extend_owner_lifetime() {
    // An orphaned daemon (parent dead) should remain alive as long as IPC
    // clients keep talking to it. Once activity stops, it exits after the
    // grace period.
    let (_root, project, runtime, state, config) = setup_project();
    let home = project.parent().expect("parent").join("home");
    let cfg = config.to_string_lossy().to_string();

    let mut up = Command::new(bin_path());
    up.current_dir(&project)
        .env("XDG_RUNTIME_DIR", &runtime)
        .env("XDG_STATE_HOME", &state)
        .env("HOME", &home)
        // Use a slightly longer grace than test B so polling slop doesn't
        // race against the watchdog.
        .env("DECOMPOSE_ORPHAN_TIMEOUT", "3")
        .arg("--file")
        .arg(&cfg)
        .arg("up")
        .arg("--table")
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    let mut child = up.spawn().expect("spawn attached up");

    assert!(
        wait_for_daemon_up_ipc(
            &project,
            &runtime,
            &state,
            &home,
            &cfg,
            Duration::from_secs(10),
        ),
        "daemon never became responsive",
    );

    // Kill the launching `up` so the daemon is orphaned.
    kill(Pid::from_raw(child.id() as i32), Signal::SIGKILL).expect("send sigkill");
    let _ = child.wait();

    // Polling a dead owner's environment must not keep it alive.
    let start = std::time::Instant::now();
    while is_daemon_live_ipc(&project, &runtime, &state, &home, &cfg) {
        assert!(
            start.elapsed() < Duration::from_secs(15),
            "IPC traffic kept orphan alive"
        );
        thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn completion_subcommand_emits_shell_scripts() {
    // No project/daemon needed — `completion` just prints to stdout.
    let tmp = tempdir().expect("tempdir");
    let project = tmp.path().join("project");
    let runtime = tmp.path().join("runtime");
    let state = tmp.path().join("state");
    let home = tmp.path().join("home");
    fs::create_dir_all(&project).expect("create project");
    fs::create_dir_all(&runtime).expect("create runtime");
    fs::create_dir_all(&state).expect("create state");
    fs::create_dir_all(&home).expect("create home");

    // Bash: should contain the clap-generated `_decompose` function and our
    // injected `complete -F __decompose_wrap ... decompose` registration.
    let bash = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["completion", "bash"],
        &[],
        &[],
    );
    assert_success(&bash, "completion bash");
    let bash_out = String::from_utf8(bash.stdout).expect("bash utf8");
    assert!(!bash_out.is_empty(), "bash completion must be non-empty");
    assert!(
        bash_out.contains("_decompose()"),
        "bash completion should define _decompose(): {bash_out}"
    );
    assert!(
        bash_out.contains("complete -F __decompose_wrap"),
        "bash completion should register the dynamic wrapper",
    );
    assert!(
        bash_out.contains("__decompose_services"),
        "bash completion should include the dynamic service helper",
    );
    assert!(
        bash_out.contains("__decompose_collect_globals"),
        "bash completion should forward global flags to decompose config --json",
    );
    assert!(
        bash_out.contains("__decompose_sessions"),
        "bash completion should include the dynamic session helper",
    );
    for cmd in [
        "up",
        "down",
        "ps",
        "attach",
        "tui",
        "logs",
        "start",
        "stop",
        "restart",
        "kill",
        "config",
        "ls",
        "run",
        "exec",
        "completion",
    ] {
        assert!(
            bash_out.contains(cmd),
            "bash completion should mention subcommand {cmd}: {bash_out}"
        );
    }

    // Zsh: should contain `#compdef decompose` and our `compdef
    // __decompose_dyn_wrap decompose` re-registration.
    let zsh = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["completion", "zsh"],
        &[],
        &[],
    );
    assert_success(&zsh, "completion zsh");
    let zsh_out = String::from_utf8(zsh.stdout).expect("zsh utf8");
    assert!(
        zsh_out.contains("#compdef decompose"),
        "zsh completion should declare #compdef",
    );
    assert!(
        zsh_out.contains("compdef __decompose_dyn_wrap decompose"),
        "zsh completion should re-register with the dynamic wrapper",
    );
    assert!(
        zsh_out.contains("__decompose_collect_globals"),
        "zsh completion should forward global flags to decompose config --json",
    );
    assert!(
        zsh_out.contains("__decompose_sessions"),
        "zsh completion should include the dynamic session helper",
    );

    // Fish: should contain `complete -c decompose ...` entries.
    let fish = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["completion", "fish"],
        &[],
        &[],
    );
    assert_success(&fish, "completion fish");
    let fish_out = String::from_utf8(fish.stdout).expect("fish utf8");
    assert!(
        fish_out.contains("complete -c decompose"),
        "fish completion should contain decompose completions",
    );
    assert!(
        fish_out.contains("__decompose_services"),
        "fish completion should include the dynamic service helper: {fish_out}",
    );
    assert!(
        fish_out.contains("__decompose_sessions"),
        "fish completion should include the dynamic session helper",
    );
    assert!(
        fish_out.contains("__decompose_collect_globals"),
        "fish completion should forward global flags",
    );
    assert!(
        fish_out.contains("__fish_seen_subcommand_from"),
        "fish completion should gate service completion on subcommand context",
    );

    // PowerShell + elvish: just assert non-empty + expected marker.
    let ps = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["completion", "powershell"],
        &[],
        &[],
    );
    assert_success(&ps, "completion powershell");
    let ps_out = String::from_utf8(ps.stdout).expect("ps utf8");
    assert!(
        ps_out.contains("Register-ArgumentCompleter"),
        "powershell completion should use Register-ArgumentCompleter",
    );
    assert!(
        ps_out.contains("__DecomposeServices"),
        "powershell completion should define the dynamic service helper",
    );
    assert!(
        ps_out.contains("__DecomposeSessions"),
        "powershell completion should define the dynamic session helper",
    );
    assert!(
        ps_out.contains("__DecomposeCollectGlobals"),
        "powershell completion should forward global flags",
    );

    let elv = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["completion", "elvish"],
        &[],
        &[],
    );
    assert_success(&elv, "completion elvish");
    let elv_out = String::from_utf8(elv.stdout).expect("elvish utf8");
    assert!(
        elv_out.contains("edit:completion:arg-completer[decompose]"),
        "elvish completion should wire decompose arg-completer",
    );
}

#[test]
fn bash_completion_completes_services_for_up_detach() {
    let mut env = TestEnv::new();
    env.with_config(
        r#"
processes:
  api:
    command: "sleep 30"
  web:
    command: "sleep 30"
"#,
    );

    let completion = env.run(&["completion", "bash"]);
    assert_success(&completion, "completion bash");

    let script_path = env.project.join("decompose.bash");
    fs::write(&script_path, completion.stdout).expect("write completion script");

    let bin_dir = Path::new(bin_path()).parent().expect("binary parent");
    let path = match std::env::var_os("PATH") {
        Some(existing) => {
            let mut paths = vec![bin_dir.to_path_buf()];
            paths.extend(std::env::split_paths(&existing));
            std::env::join_paths(paths).expect("join PATH")
        }
        None => bin_dir.as_os_str().to_os_string(),
    };
    let script = format!(
        r#"
PATH={:?}
complete() {{ :; }}
compgen() {{
    local words="" prefix=""
    while [[ $# -gt 0 ]]; do
        case "$1" in
            -W) words="$2"; shift 2;;
            --) prefix="$2"; shift 2;;
            *) shift;;
        esac
    done
    for w in $words; do
        [[ $w == "$prefix"* ]] && printf '%s\n' "$w"
    done
}}
source {:?}
COMP_WORDS=(decompose up -d "")
COMP_CWORD=3
__decompose_wrap
printf '%s\n' "${{COMPREPLY[@]}}"
"#,
        bin_dir, script_path
    );

    let out = Command::new("bash")
        .arg("-lc")
        .arg(script)
        .current_dir(&env.project)
        .env("PATH", path)
        .env("XDG_RUNTIME_DIR", &env.runtime)
        .env("XDG_STATE_HOME", &env.state)
        .env("HOME", &env.home)
        .output()
        .expect("run bash completion");
    assert_success(&out, "bash completion for up -d");

    let completed = String::from_utf8(out.stdout).expect("completion stdout utf8");
    let services: Vec<&str> = completed.lines().collect();
    assert!(
        services.contains(&"api") && services.contains(&"web"),
        "expected api and web service completions, got: {services:?}"
    );
}

#[test]
fn completion_rejects_unknown_shell() {
    let tmp = tempdir().expect("tempdir");
    let project = tmp.path().join("project");
    let runtime = tmp.path().join("runtime");
    let state = tmp.path().join("state");
    let home = tmp.path().join("home");
    fs::create_dir_all(&project).expect("create project");
    fs::create_dir_all(&runtime).expect("create runtime");
    fs::create_dir_all(&state).expect("create state");
    fs::create_dir_all(&home).expect("create home");

    let out = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["completion", "tcsh"],
        &[],
        &[],
    );
    assert!(
        !out.status.success(),
        "completion with unknown shell should fail"
    );
}

// ---------------------------------------------------------------------------
// Disabled-flag integration tests
//
// These pin down the end-to-end behaviour of the `disabled: true` YAML flag:
//   - `up` must skip disabled services (supervisor filter in daemon.rs).
//   - `ps` must surface `state: "disabled"` with no pid.
//   - `start` against a disabled service flips it out of terminal state.
//   - Reload (via a second `up`) toggling disabled true↔false should take
//     effect.
//
// Several of these tests assert *current* behaviour rather than ideal
// behaviour; see the inline comments for the surprises that motivated
// follow-up work.
// ---------------------------------------------------------------------------

/// Helper: find the named process's entry in a `ps --json` payload and return
/// its `(state, pid)`. Panics if the entry is missing — the tests below all
/// write configs where every service should be present.
fn state_and_pid_of(ps_json: &Value, name: &str) -> (String, Option<u64>) {
    let proc = ps_json
        .get("processes")
        .and_then(Value::as_array)
        .expect("ps processes array")
        .iter()
        .find(|p| p.get("name").and_then(Value::as_str) == Some(name))
        .unwrap_or_else(|| panic!("service {name:?} missing from ps"));
    let state = proc
        .get("state")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let pid = proc.get("pid").and_then(Value::as_u64);
    (state, pid)
}

/// Poll `ps --json` until `predicate(state, pid)` returns true for `name`, or
/// the deadline elapses. Returns the final observed (state, pid) regardless of
/// whether the predicate matched — callers assert on the shape they expected.
fn wait_for_state(
    env: &TestEnv,
    name: &str,
    timeout: Duration,
    predicate: impl Fn(&str, Option<u64>) -> bool,
) -> (String, Option<u64>) {
    let deadline = std::time::Instant::now() + timeout;
    let mut last = (String::new(), None);
    while std::time::Instant::now() < deadline {
        let parsed = env.ps_json_value();
        last = state_and_pid_of(&parsed, name);
        if predicate(&last.0, last.1) {
            return last;
        }
        thread::sleep(Duration::from_millis(100));
    }
    last
}

/// `up -d --wait` with one enabled and one disabled service:
///   * the enabled service reaches Running and has a pid.
///   * the disabled service sits in `state: "disabled"` with no pid — the
///     supervisor's skip at daemon.rs:532 prevents it from launching.
#[test]
fn disabled_up_skips_service() {
    let mut env = TestEnv::new();
    env.with_config(
        r#"
processes:
  alive:
    command: "sleep 30"
  dead:
    command: "sleep 30"
    disabled: true
"#,
    );

    let up = env.run(&["up", "-d", "--wait", "--json"]);
    assert_success(&up, "up -d --wait");
    env.up_started = true;

    let parsed = env.ps_json_value();
    let (alive_state, alive_pid) = state_and_pid_of(&parsed, "alive");
    assert_eq!(alive_state, "running", "alive should be running");
    assert!(alive_pid.is_some(), "alive should have a pid");

    let (dead_state, dead_pid) = state_and_pid_of(&parsed, "dead");
    assert_eq!(dead_state, "disabled", "dead should report disabled state");
    assert!(
        dead_pid.is_none(),
        "dead should have no pid, got {dead_pid:?}"
    );
}

/// Convenience pair to the above: `ps` output lists disabled services (they
/// aren't hidden) and surfaces the canonical `"disabled"` state string so
/// downstream consumers (table, JSON, `--wait` filter) can distinguish them
/// from `not_started`.
#[test]
fn disabled_ps_shows_disabled_state() {
    let mut env = TestEnv::new();
    env.with_config(
        r#"
processes:
  only_disabled:
    command: "sleep 30"
    disabled: true
"#,
    );

    env.up_detach_json();

    let parsed = env.ps_json_value();
    let procs = parsed
        .get("processes")
        .and_then(Value::as_array)
        .expect("ps processes");
    assert_eq!(procs.len(), 1, "disabled service must still appear in ps");

    let (state, pid) = state_and_pid_of(&parsed, "only_disabled");
    assert_eq!(state, "disabled");
    assert!(pid.is_none());
}

/// `decompose start <disabled-svc>` attempts to transition the service out
/// of its terminal `Disabled` state. `handle_start` flips terminal statuses
/// to `Pending`, but the supervisor's per-tick filter in `supervisor_loop`
/// also skips any runtime whose `spec.disabled == true`. Explicit `start`
/// clears `spec.disabled` as an override so the supervisor picks it up.
#[test]
fn disabled_start_transitions_to_running() {
    let mut env = TestEnv::new();
    env.with_config(
        r#"
processes:
  dead:
    command: "sleep 30"
    disabled: true
"#,
    );

    env.up_detach_json();

    // Baseline: dead is Disabled and has no pid.
    let parsed = env.ps_json_value();
    let (state, pid) = state_and_pid_of(&parsed, "dead");
    assert_eq!(state, "disabled");
    assert!(pid.is_none());

    // Ask the daemon to start the disabled service.
    let start = env.run(&["start", "--json", "dead"]);
    assert_success(&start, "start dead");

    // Start clears spec.disabled and moves Disabled → Pending; the
    // supervisor then launches the service.
    let (final_state, final_pid) = wait_for_state(&env, "dead", Duration::from_secs(3), |s, p| {
        s == "running" && p.is_some()
    });
    assert_eq!(
        final_state, "running",
        "expected running after start on disabled svc, got {final_state:?} pid={final_pid:?}"
    );
    assert!(final_pid.is_some(), "service must have a pid after start");
}

/// `start A` where `A depends_on: B` and `B` is `disabled: true`.
///
/// `handle_start`'s transitive-deps walk also overrides disabled on deps:
/// `start A` is treated as an explicit intent to bring up everything `A`
/// needs, which is more consistent than stalling the whole chain.
#[test]
fn disabled_start_respects_other_disabled_deps() {
    let mut env = TestEnv::new();
    env.with_config(
        r#"
processes:
  dep:
    command: "sleep 30"
    disabled: true
  app:
    command: "sleep 30"
    depends_on:
      dep:
        condition: process_started
"#,
    );

    env.up_detach_json();

    // Baseline: both services parked, no pids.
    let parsed = env.ps_json_value();
    let (dep_state, dep_pid) = state_and_pid_of(&parsed, "dep");
    let (_, app_pid) = state_and_pid_of(&parsed, "app");
    assert_eq!(dep_state, "disabled");
    assert!(dep_pid.is_none());
    assert!(app_pid.is_none());

    // Ask the daemon to start app. The transitive walk adds `dep`, clears
    // spec.disabled on both, and the supervisor launches dep → app.
    let start = env.run(&["start", "--json", "app"]);
    assert_success(&start, "start app");

    let (dep_state, dep_pid) = wait_for_state(&env, "dep", Duration::from_secs(3), |s, p| {
        s == "running" && p.is_some()
    });
    assert_eq!(dep_state, "running", "dep should launch; got {dep_state:?}");
    assert!(dep_pid.is_some());

    let (app_state, app_pid) = wait_for_state(&env, "app", Duration::from_secs(3), |s, p| {
        s == "running" && p.is_some()
    });
    assert_eq!(app_state, "running", "app should launch; got {app_state:?}");
    assert!(app_pid.is_some());
}

/// Reload toggling `disabled: true → false` via a second `up`.
///
/// Reload handles a pure `disabled` toggle as its own dimension: the
/// existing runtime is stopped (true → ...) or flipped to Pending
/// (... → false) without a recreate.
#[test]
fn disabled_reload_toggles_true_to_false() {
    let mut env = TestEnv::new();
    env.with_config(
        r#"
processes:
  toggler:
    command: "sleep 30"
    disabled: true
"#,
    );

    env.up_detach_json();

    // Baseline: disabled, no pid.
    let parsed = env.ps_json_value();
    let (state, pid) = state_and_pid_of(&parsed, "toggler");
    assert_eq!(state, "disabled");
    assert!(pid.is_none());

    // Flip disabled → false in the config and re-run up. `up` on an
    // existing daemon sends Reload followed by Start.
    env.with_config(
        r#"
processes:
  toggler:
    command: "sleep 30"
"#,
    );
    let up2 = env.run(&["up", "-d", "--json"]);
    assert_success(&up2, "second up with disabled: false");

    // Reload flips Disabled → Pending; supervisor launches it.
    let (state_after, pid_after) =
        wait_for_state(&env, "toggler", Duration::from_secs(3), |s, p| {
            s == "running" && p.is_some()
        });
    assert_eq!(
        state_after, "running",
        "toggler should be running after disabled=false reload, got {state_after:?}"
    );
    let first_pid = pid_after.expect("running service must have a pid");

    // Now toggle back to disabled: true. Reload stops the running instance
    // and flips it to Disabled.
    env.with_config(
        r#"
processes:
  toggler:
    command: "sleep 30"
    disabled: true
"#,
    );
    let up3 = env.run(&["up", "-d", "--json"]);
    assert_success(&up3, "third up back to disabled: true");

    let (state_final, pid_final) =
        wait_for_state(&env, "toggler", Duration::from_secs(3), |s, _| {
            s == "disabled"
        });
    assert_eq!(
        state_final, "disabled",
        "toggler should be disabled after reload, got {state_final:?}"
    );
    assert!(
        pid_final.is_none(),
        "disabled service must have no pid after toggle (had pid {first_pid} before)"
    );
}

/// `--remove-orphans` must also clean up services that have already
/// exited (e.g. a command that ran once and finished): the previous
/// behaviour left the stale runtime in the daemon state, confusing `ps`.
#[test]
fn reload_with_remove_orphans_cleans_failed_service() {
    let (_root, project, runtime, state, _config) = setup_project();
    let home = project.parent().expect("parent").join("home");
    let cfg_path = project.join("decompose.yaml");
    rewrite_config(
        &cfg_path,
        r#"
processes:
  alpha:
    command: "sleep 30"
  dud:
    command: "sh -c 'exit 3'"
"#,
    );
    let cfg = cfg_path.to_string_lossy().to_string();

    let up1 = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "up", "-d", "--json"],
        &[],
        &[],
    );
    assert_success(&up1, "first up with failing service");

    // Wait for `dud` to exit with a failure.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let ps = run_cmd(
            &project,
            &runtime,
            &state,
            &home,
            &["--file", &cfg, "ps", "--json"],
            &[],
            &[],
        );
        let parsed: Value = serde_json::from_slice(&ps.stdout).expect("ps json");
        if state_of(&parsed, "dud").as_deref() == Some("failed") {
            break;
        }
        if std::time::Instant::now() > deadline {
            panic!("dud never reached failed state: {parsed}");
        }
        thread::sleep(Duration::from_millis(50));
    }

    // Remove dud from config and reload with --remove-orphans.
    rewrite_config(
        &cfg_path,
        r#"
processes:
  alpha:
    command: "sleep 30"
"#,
    );
    let up2 = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "up", "-d", "--json", "--remove-orphans"],
        &[],
        &[],
    );
    assert_success(&up2, "second up with --remove-orphans");

    let ps = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "ps", "--json"],
        &[],
        &[],
    );
    let parsed: Value = serde_json::from_slice(&ps.stdout).expect("ps json");
    let procs = parsed.get("processes").and_then(Value::as_array).unwrap();
    assert_eq!(
        procs.len(),
        1,
        "failed orphan should be cleaned up, got: {parsed}"
    );
    assert_eq!(state_of(&parsed, "alpha").as_deref(), Some("running"));

    let down = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "down", "--json"],
        &[],
        &[],
    );
    assert_success(&down, "down");
}

/// Editing .env between `up` runs must recreate the service so the new
/// value reaches the child. The fix hinges on including the resolved
/// post-merge env in compute_config_hash — otherwise the reload diff
/// treats the service as unchanged and never restarts it.
#[test]
fn dotenv_changes_propagate_to_running_process_on_reload() {
    let mut env = TestEnv::new();
    let outfile = env.project.join("out.txt");
    let script = env.project.join("write_token.sh");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf '%s' \"$API_TOKEN\" > {out}\nexec sleep 30\n",
            out = outfile.display()
        ),
    )
    .expect("write script");
    let mut perms = fs::metadata(&script).unwrap().permissions();
    use std::os::unix::fs::PermissionsExt;
    perms.set_mode(0o755);
    fs::set_permissions(&script, perms).expect("chmod");

    let yaml = format!(
        r#"
processes:
  svc:
    command: "{script}"
"#,
        script = script.display()
    );
    env.with_config(&yaml);
    fs::write(env.project.join(".env"), "API_TOKEN=first\n").expect("write .env");

    env.up_detach_json();

    // Wait for the service to have written to the file.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !outfile.exists() && std::time::Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }
    assert!(outfile.exists(), "service never wrote output file");
    let first = fs::read_to_string(&outfile)
        .expect("read out")
        .trim()
        .to_string();
    assert_eq!(first, "first", "baseline value must come from .env");

    // Edit .env and re-run up. Reload should detect the env change via the
    // hash and recreate svc; the new child writes the new value.
    fs::write(env.project.join(".env"), "API_TOKEN=second\n").expect("update .env");
    fs::remove_file(&outfile).expect("clear output");

    let up2 = env.run(&["up", "-d", "--json"]);
    assert_success(&up2, "second up after .env edit");

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut observed = String::new();
    while std::time::Instant::now() < deadline {
        if let Ok(contents) = fs::read_to_string(&outfile) {
            observed = contents.trim().to_string();
            if observed == "second" {
                break;
            }
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        observed, "second",
        "after .env edit + up, child must see the new value"
    );
}

/// The daemon log file must be truncated on each `up` so `decompose logs`
/// only shows the current session. See `decompose-eii`.
#[test]
fn daemon_log_is_truncated_on_each_up() {
    let (_root, project, runtime, state, _config) = setup_project();
    let home = project.parent().expect("parent").join("home");

    let cfg_path = project.join("decompose.yaml");
    let cfg = cfg_path.to_string_lossy().to_string();

    // First session emits SESSION_ONE.
    fs::write(
        &cfg_path,
        r#"
processes:
  talker:
    command: "sh -c 'echo SESSION_ONE_MARKER; sleep 30'"
"#,
    )
    .expect("write config 1");

    let up1 = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "up", "--detach", "--json"],
        &[],
        &[],
    );
    assert_success(&up1, "up session 1");

    // Wait for the first session's marker to land in the log.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut saw_first = false;
    while std::time::Instant::now() < deadline {
        let logs = run_cmd(
            &project,
            &runtime,
            &state,
            &home,
            &["--file", &cfg, "logs", "--no-pager"],
            &[],
            &[],
        );
        if logs.status.success()
            && String::from_utf8_lossy(&logs.stdout).contains("SESSION_ONE_MARKER")
        {
            saw_first = true;
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    assert!(saw_first, "expected SESSION_ONE_MARKER after first up");

    let down1 = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "down", "--json"],
        &[],
        &[],
    );
    assert_success(&down1, "down session 1");

    // Swap in a second command and bring the env back up.
    fs::write(
        &cfg_path,
        r#"
processes:
  talker:
    command: "sh -c 'echo SESSION_TWO_MARKER; sleep 30'"
"#,
    )
    .expect("write config 2");

    let up2 = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "up", "--detach", "--json"],
        &[],
        &[],
    );
    assert_success(&up2, "up session 2");

    // Wait for the second session's marker, then assert the first is gone.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut final_text = String::new();
    while std::time::Instant::now() < deadline {
        let logs = run_cmd(
            &project,
            &runtime,
            &state,
            &home,
            &["--file", &cfg, "logs", "--no-pager"],
            &[],
            &[],
        );
        if logs.status.success() {
            let text = String::from_utf8_lossy(&logs.stdout).to_string();
            if text.contains("SESSION_TWO_MARKER") {
                final_text = text;
                break;
            }
        }
        thread::sleep(Duration::from_millis(100));
    }
    assert!(
        final_text.contains("SESSION_TWO_MARKER"),
        "expected SESSION_TWO_MARKER after second up, got:\n{final_text}"
    );
    assert!(
        !final_text.contains("SESSION_ONE_MARKER"),
        "daemon log should be truncated on new up; stale content leaked:\n{final_text}"
    );

    let down2 = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "down", "--json"],
        &[],
        &[],
    );
    assert_success(&down2, "down session 2");
}

#[test]
fn structured_logs_isolate_replicas_and_drain_long_unterminated_output() {
    let (root, project, runtime, state, config) = setup_project();
    let home = root.path().join("home");
    let payload = format!("{}\nEND_LONG\n", "é".repeat(70_000));
    fs::write(project.join("payload"), &payload).unwrap();
    fs::write(
        &config,
        r#"
processes:
  alpha:
    replicas: 2
    command: "echo READY; cat payload; echo ERR >&2; printf FINAL; sleep 30"
    ready_log_line: READY
  beta:
    command: "echo BETA_ONLY; sleep 30"
"#,
    )
    .unwrap();
    let run = |args: &[&str]| run_cmd(&project, &runtime, &state, &home, args, &[], &[]);
    assert_success(&run(&["up", "-d", "--wait"]), "up");
    let checks = std::panic::catch_unwind(|| {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let logs = run(&["logs", "--no-pager", "alpha[1]"]);
            assert_success(&logs, "replica logs");
            let text = String::from_utf8_lossy(&logs.stdout);
            assert!(!text.contains("BETA_ONLY"));
            assert!(
                !text.contains("[alpha"),
                "single replica strips display prefix"
            );
            if text.contains("END_LONG") && text.contains("ERR") {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "missing replica output"
            );
            thread::sleep(Duration::from_millis(50));
        }
        let logs = run(&["logs", "--no-pager", "beta"]);
        assert_success(&logs, "beta logs");
        let text = String::from_utf8_lossy(&logs.stdout);
        assert!(text.lines().any(|line| line == "BETA_ONLY"));
        assert!(!text.contains("END_LONG"));
    });
    assert_success(&run(&["down"]), "down drains final output");
    if let Err(panic) = checks {
        std::panic::resume_unwind(panic);
    }

    let state_root = state.join("decompose");
    let diagnostic = fs::read_dir(&state_root)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().is_some_and(|e| e == "log"))
        .unwrap();
    let diagnostics = fs::read_to_string(&diagnostic).unwrap();
    assert!(!diagnostics.contains("END_LONG"));
    assert!(!diagnostics.contains("BETA_ONLY"));
    let log_dir = diagnostic.with_extension("").join("logs");
    let mut files = 0;
    for entry in fs::read_dir(log_dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|e| e != "jsonl") {
            continue;
        }
        files += 1;
        let contents = fs::read_to_string(&path).unwrap();
        let records: Vec<Value> = contents
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert!(records.iter().all(|r| r.get("seq").is_none()));
        assert!(
            records
                .iter()
                .all(|r| humantime::parse_rfc3339(r["timestamp"].as_str().unwrap()).is_ok())
        );
        let service = records[0]["service"].as_str().unwrap();
        let replica = records[0]["replica"].as_u64().unwrap();
        assert!(
            records
                .iter()
                .all(|r| r["service"] == service && r["replica"] == replica)
        );
        if service == "alpha" {
            assert!(records.iter().any(|r| r["partial"] == true));
            assert!(
                records
                    .iter()
                    .any(|r| r["stream"] == "stderr" && r["message"] == "ERR")
            );
            let stdout: String = records
                .iter()
                .filter(|r| r["stream"] == "stdout")
                .map(|r| r["message"].as_str().unwrap())
                .collect();
            assert_eq!(stdout, format!("READY{}END_LONGFINAL", "é".repeat(70_000)));
        }
    }
    assert_eq!(files, 3, "one output file per replica");
}

// Startup hooks use synchronization files for lifecycle races. Polling only
// observes a condition; it never stands in for completion of an operation.
fn wait_hook_snapshot(env: &TestEnv, condition: impl Fn(&Value) -> bool) -> Value {
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        let ps = env.ps_json_value();
        if condition(&ps) {
            return ps;
        }
        assert!(std::time::Instant::now() < deadline, "timed out: {ps}");
        thread::sleep(Duration::from_millis(30));
    }
}
fn hook_service<'a>(ps: &'a Value, name: &str) -> &'a Value {
    ps["processes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == name)
        .unwrap()
}

#[test]
fn startup_hooks_acceptance_guards_restart_and_reinitialize() {
    let mut env = TestEnv::new();
    env.with_config(
        r#"
processes:
  db:
    command: 'test -f data/prepared && touch endpoint; exec sleep 100'
    pre_start:
      - name: prepare
        creates: data/prepared
        command: 'mkdir -p data; echo pre >> mutations; touch data/prepared'
    post_start:
      - name: user
        wait_for: {exec: {command: 'test -f endpoint'}}
        unless: 'test -f data/user'
        command: 'echo post >> mutations; touch data/user'
  api:
    command: 'test -f data/user; exec sleep 100'
    depends_on: {db: {condition: process_initialized}}
"#,
    );
    env.up_detach_json();
    assert_success(&env.run(&["up", "-d", "--wait"]), "wait initialization");
    assert_eq!(
        fs::read_to_string(env.project.join("mutations")).unwrap(),
        "pre\npost\n"
    );
    assert_success(&env.run(&["restart", "db"]), "restart");
    wait_hook_snapshot(&env, |ps| {
        hook_service(ps, "db")["initialization"]["initialized"] == true
    });
    let ps = env.ps_json_value();
    for h in hook_service(&ps, "db")["initialization"]["hooks"]
        .as_array()
        .unwrap()
    {
        assert_eq!(h["reason"], "already_satisfied");
    }
    assert_eq!(
        fs::read_to_string(env.project.join("mutations")).unwrap(),
        "pre\npost\n"
    );
    assert_success(&env.run(&["stop", "db"]), "stop");
    wait_hook_snapshot(&env, |ps| hook_service(ps, "db")["state"] == "stopped");
    fs::remove_dir_all(env.project.join("data")).unwrap();
    assert_success(&env.run(&["start", "db"]), "start");
    wait_hook_snapshot(&env, |ps| {
        hook_service(ps, "db")["initialization"]["initialized"] == true
    });
    assert_eq!(
        fs::read_to_string(env.project.join("mutations")).unwrap(),
        "pre\npost\npre\npost\n"
    );
    env.down_json();
}

#[test]
fn startup_hooks_pre_failure_prevents_spawn_and_wait_fails_promptly() {
    let mut env = TestEnv::new();
    env.with_config(
        r#"
processes:
  db:
    command: 'touch spawned; sleep 100'
    restart_policy: always
    pre_start:
      - {name: broken, unless: 'exit 2', command: 'touch mutation'}
      - {name: later, command: 'touch later'}
    post_start: [{name: post, command: 'touch post'}]
  api:
    command: 'sleep 100'
    depends_on: {db: {condition: process_initialized}}
"#,
    );
    env.up_detach_json();
    let ps = wait_hook_snapshot(&env, |ps| {
        hook_service(ps, "db")["state"] == "failed_to_start"
    });
    let db = hook_service(&ps, "db");
    assert!(db["pid"].is_null());
    assert_eq!(db["restart_count"], 0);
    assert_eq!(db["initialization"]["hooks"][0]["exit_code"], 2);
    assert_eq!(db["initialization"]["hooks"][1]["reason"], "prior_failure");
    assert_eq!(hook_service(&ps, "api")["state"], "pending");
    assert_eq!(
        hook_service(&ps, "api")["initialization_blockers"][0]["service"],
        "db"
    );
    for path in ["spawned", "mutation", "later", "post"] {
        assert!(!env.project.join(path).exists());
    }
    let out = env.run(&["up", "-d", "--wait"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("pre_start:broken"));
}

#[test]
fn startup_hooks_post_failure_keeps_healthy_child_and_is_visible() {
    let mut env = TestEnv::new();
    env.with_config(
        r#"
processes:
  db:
    command: 'sleep 100'
    readiness_probe: {exec: {command: 'exit 0'}, period_seconds: 1}
    post_start:
      - {name: missing, creates: promised, command: 'exit 0'}
      - {name: later, command: 'touch later'}
"#,
    );
    env.up_detach_json();
    let ps = wait_hook_snapshot(&env, |ps| {
        let p = hook_service(ps, "db");
        p["ready"] == true && p["initialization"]["state"] == "failed"
    });
    let db = hook_service(&ps, "db");
    assert_eq!(db["state"], "running");
    assert_eq!(db["initialization"]["initialized"], false);
    assert_eq!(db["initialization"]["hooks"][0]["stage"], "verifying");
    assert!(!env.project.join("promised").exists());
    let table = env.run(&["ps", "--table"]);
    assert!(
        String::from_utf8_lossy(&table.stdout)
            .contains("initialization failed (post_start:missing")
    );
}

#[test]
fn startup_hooks_slow_pre_is_concurrent_and_cancelled_before_spawn() {
    let mut env = TestEnv::new();
    env.with_config(
        r#"
processes:
  a_slow:
    command: 'touch spawned; sleep 100'
    pre_start:
      - name: waiting
        command: 'echo $$$$ > hook.pid; sleep 100 & echo $! > grandchild.pid; wait'
  z_fast:
    command: 'sleep 100'
"#,
    );
    env.up_detach_json();
    let ps = wait_hook_snapshot(&env, |ps| {
        env.project.join("grandchild.pid").exists()
            && hook_service(ps, "z_fast")["state"] == "running"
    });
    assert_eq!(hook_service(&ps, "a_slow")["state"], "initializing");
    assert!(hook_service(&ps, "a_slow")["pid"].is_null());
    let pid: u32 = fs::read_to_string(env.project.join("grandchild.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_success(&env.run(&["stop", "a_slow"]), "stop pre hook");
    let ps = wait_hook_snapshot(&env, |ps| hook_service(ps, "a_slow")["state"] == "stopped");
    assert_eq!(
        hook_service(&ps, "a_slow")["initialization"]["state"],
        "cancelled"
    );
    assert!(!process_alive(pid));
    assert!(!env.project.join("spawned").exists());
    env.down_json();
}

#[test]
fn startup_hooks_child_exit_cancels_post_without_losing_exit_status() {
    let mut env = TestEnv::new();
    env.with_config(
        r#"
processes:
  job:
    command: 'while ! test -f hook.pid; do sleep 0.02; done; exit 7'
    post_start:
      - {name: long, command: 'echo $$$$ > hook.pid; exec sleep 100'}
      - {name: never, command: 'touch never'}
"#,
    );
    env.up_detach_json();
    let ps = wait_hook_snapshot(&env, |ps| hook_service(ps, "job")["state"] == "failed");
    let p = hook_service(&ps, "job");
    assert_eq!(p["exit_code"], 7);
    assert_eq!(p["initialization"]["state"], "cancelled");
    assert_eq!(p["initialization"]["hooks"][1]["reason"], "service_exited");
    assert!(!env.project.join("never").exists());
    env.down_json();
}

#[test]
fn startup_hooks_cancel_every_stage_in_both_phases() {
    for phase in ["pre_start", "post_start"] {
        for (stage, operation) in [
            ("waiting", "stop"),
            ("checking", "down"),
            ("executing", "kill"),
            ("verifying", "restart"),
        ] {
            let mut env = TestEnv::new();
            let fields = match stage {
                "waiting" => {
                    "wait_for: {exec: {command: 'echo $$ > hook.pid; exec sleep 100'}, timeout_seconds: 30}\n        command: 'touch mutation'"
                }
                "checking" => {
                    "unless: 'echo $$ > hook.pid; exec sleep 100'\n        command: 'touch mutation'"
                }
                "executing" => "command: 'echo $$ > hook.pid; exec sleep 100'",
                _ => {
                    "unless: 'if test -f marker; then echo $$ > hook.pid; exec sleep 100; else exit 1; fi'\n        command: 'touch marker'"
                }
            };
            env.with_config(&format!("disable_env_expansion: true\nprocesses:\n  svc:\n    command: 'touch spawned; exec sleep 100'\n    {phase}:\n      - name: active\n        {fields}\n      - name: later\n        command: 'touch later'\n"));
            env.up_detach_json();
            wait_hook_snapshot(&env, |ps| {
                env.project.join("hook.pid").exists()
                    && hook_service(ps, "svc")["initialization"]["hooks"][0]["stage"] == stage
            });
            let pid: u32 = fs::read_to_string(env.project.join("hook.pid"))
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            assert_success(
                &env.run(&[operation, "--json"]),
                &format!("{operation} {phase} {stage}"),
            );
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while process_alive(pid) {
                assert!(
                    std::time::Instant::now() < deadline,
                    "leaked {phase} {stage} subprocess {pid}"
                );
                thread::sleep(Duration::from_millis(20));
            }
            assert!(!env.project.join("later").exists());
            if phase == "pre_start" {
                assert!(!env.project.join("spawned").exists());
            }
            if operation != "down" {
                env.down_json();
            } else {
                env.up_started = false;
            }
        }
    }
}

#[test]
fn startup_hooks_guard_contracts_and_artifact_errors() {
    for (guard, command, succeeds, reason) in [
        (
            "unless: 'exit 0'",
            "touch mutation",
            true,
            "already_satisfied",
        ),
        ("unless: 'exit 1'", "touch mutation", false, ""),
        ("unless: 'kill -TERM $$$$'", "touch mutation", false, ""),
        ("creates: artifact", "exit 4", false, ""),
        ("creates: dangling", "touch target", true, ""),
        (
            "creates: present",
            "touch mutation",
            true,
            "already_satisfied",
        ),
        (
            "creates: inaccessible/artifact",
            "touch mutation",
            false,
            "",
        ),
    ] {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let mut env = TestEnv::new();
        symlink("target", env.project.join("dangling")).unwrap();
        fs::create_dir(env.project.join("present")).unwrap();
        fs::create_dir(env.project.join("inaccessible")).unwrap();
        fs::set_permissions(
            env.project.join("inaccessible"),
            fs::Permissions::from_mode(0o0),
        )
        .unwrap();
        env.with_config(&format!("processes:\n  svc:\n    command: 'sleep 100'\n    pre_start:\n      - name: guarded\n        {guard}\n        command: '{command}'\n"));
        env.up_detach_json();
        let ps = wait_hook_snapshot(&env, |ps| {
            matches!(
                hook_service(ps, "svc")["initialization"]["state"].as_str(),
                Some("succeeded" | "failed")
            )
        });
        fs::set_permissions(
            env.project.join("inaccessible"),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        let init = &hook_service(&ps, "svc")["initialization"];
        assert_eq!(init["state"] == "succeeded", succeeds, "{guard}: {init}");
        if !reason.is_empty() {
            assert_eq!(init["hooks"][0]["reason"], reason);
            assert!(!env.project.join("mutation").exists());
        }
        if guard.contains("artifact") {
            assert!(!env.project.join("artifact").exists());
        }
    }
}

#[test]
fn startup_hooks_wait_retries_cleanup_and_overall_timeout() {
    let mut env = TestEnv::new();
    env.with_config(
        r#"
disable_env_expansion: true
processes:
  svc:
    command: 'touch spawned; sleep 100'
    pre_start:
      - name: bounded
        timeout_seconds: 3
        wait_for:
          exec: {command: 'echo $$ >> probes; sleep 100 & echo $! >> grandchildren; wait'}
          timeout_seconds: 1
        command: 'touch mutation'
"#,
    );
    env.up_detach_json();
    let started = std::time::Instant::now();
    let ps = wait_hook_snapshot(&env, |ps| {
        hook_service(ps, "svc")["state"] == "failed_to_start"
    });
    assert!(started.elapsed() < Duration::from_secs(9));
    assert_eq!(
        hook_service(&ps, "svc")["initialization"]["hooks"][0]["stage"],
        "waiting"
    );
    let probes = fs::read_to_string(env.project.join("probes")).unwrap();
    assert!(probes.lines().count() >= 2);
    for path in ["probes", "grandchildren"] {
        for pid in fs::read_to_string(env.project.join(path)).unwrap().lines() {
            assert!(!process_alive(pid.parse().unwrap()), "leaked probe {pid}");
        }
    }
    assert!(!env.project.join("mutation").exists());
    assert!(!env.project.join("spawned").exists());
}

#[test]
fn startup_hooks_deadline_covers_check_command_and_verification() {
    let mut env = TestEnv::new();
    env.with_config(
        r#"
processes:
  svc:
    command: 'sleep 100'
    pre_start:
      - name: shared-deadline
        timeout_seconds: 1
        unless: 'if test -f mutation; then sleep 100; else exit 1; fi'
        command: 'touch mutation'
"#,
    );
    env.up_detach_json();
    let ps = wait_hook_snapshot(&env, |ps| {
        hook_service(ps, "svc")["state"] == "failed_to_start"
    });
    assert_eq!(
        hook_service(&ps, "svc")["initialization"]["hooks"][0]["stage"],
        "verifying"
    );
    assert!(fs::read_to_string(env.project.join("mutation")).is_ok());
}

#[test]
fn startup_hooks_dependency_order_and_readiness_are_independent() {
    let mut env = TestEnv::new();
    env.with_config(r#"
processes:
  dependency:
    command: 'touch dependency; exec sleep 100'
    ready_log_line: MAIN_READY
    post_start:
      - name: independent
        command: 'echo MAIN_READY; echo hook-error >&2; touch hook-output; while ! test -f release; do sleep 0.02; done'
  started:
    command: 'sleep 100'
    depends_on: {dependency: {condition: process_started}}
    pre_start: [{name: check-dep, command: 'test -f dependency; touch started-pre'}]
  initialized:
    command: 'sleep 100'
    depends_on: {dependency: {condition: process_initialized}}
    pre_start: [{name: check-init, command: 'test -f release; touch initialized-pre'}]
"#);
    env.up_detach_json();
    let ps = wait_hook_snapshot(&env, |_| {
        env.project.join("hook-output").exists() && env.project.join("started-pre").exists()
    });
    assert_eq!(hook_service(&ps, "dependency")["log_ready"], false);
    assert_eq!(hook_service(&ps, "initialized")["state"], "pending");
    assert!(!env.project.join("initialized-pre").exists());
    fs::write(env.project.join("release"), "").unwrap();
    wait_hook_snapshot(&env, |_| env.project.join("initialized-pre").exists());
    let logs = env.run(&["logs", "--no-pager", "dependency"]);
    let logs = String::from_utf8_lossy(&logs.stdout);
    assert!(
        logs.contains("[post_start:independent] MAIN_READY"),
        "{logs}"
    );
    assert!(logs.contains("hook-error"));
    let mut records = Vec::new();
    fn collect_logs(dir: &Path, records: &mut Vec<Value>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                collect_logs(&path, records);
            } else if path.extension().is_some_and(|s| s == "jsonl") {
                for line in fs::read_to_string(path).unwrap().lines() {
                    records.push(serde_json::from_str(line).unwrap());
                }
            }
        }
    }
    collect_logs(&env.state, &mut records);
    assert!(records.iter().any(|r| r["hook_name"] == "independent"
        && r["hook_stage"] == "executing"
        && r["stream"] == "stderr"
        && r["message"] == "hook-error"));
    env.down_json();
}

#[test]
fn startup_hooks_replicas_live_rename_scale_down_and_reload() {
    let mut env = TestEnv::new();
    let config = |replicas, command: &str| {
        format!(
            r#"
processes:
  worker:
    replicas: {replicas}
    command: 'sleep 100'
    post_start:
      - name: work
        command: '{command}'
  client:
    command: 'sleep 100'
    depends_on: {{worker: {{condition: process_initialized}}}}
"#
        )
    };
    env.with_config(&config(
        1,
        "echo start >> starts; while ! test -f release; do sleep 0.02; done; echo done",
    ));
    env.up_detach_json();
    let ps = wait_hook_snapshot(&env, |_| env.project.join("starts").exists());
    let original = hook_service(&ps, "worker")["pid"].clone();
    env.with_config(&config(
        2,
        "echo start >> starts; while ! test -f release; do sleep 0.02; done; echo done",
    ));
    assert_success(&env.run(&["up", "-d"]), "scale up");
    let ps = wait_hook_snapshot(&env, |_| {
        fs::read_to_string(env.project.join("starts"))
            .unwrap()
            .lines()
            .count()
            == 2
    });
    assert_eq!(hook_service(&ps, "worker[1]")["pid"], original);
    assert_eq!(hook_service(&ps, "client")["state"], "pending");
    env.with_config(&config(
        1,
        "echo start >> starts; while ! test -f release; do sleep 0.02; done; echo done",
    ));
    assert_success(&env.run(&["up", "-d"]), "scale down active hook");
    fs::write(env.project.join("release"), "").unwrap();
    let ps = wait_hook_snapshot(&env, |ps| hook_service(ps, "client")["state"] == "running");
    assert_eq!(hook_service(&ps, "worker")["pid"], original);
    let logs = env.run(&["logs", "--no-pager", "worker"]);
    assert!(String::from_utf8_lossy(&logs.stdout).contains("[post_start:work] done"));
    assert_success(&env.run(&["up", "-d", "--wait"]), "unchanged up");
    assert_eq!(
        fs::read_to_string(env.project.join("starts"))
            .unwrap()
            .lines()
            .count(),
        2
    );
    env.with_config(&config(1, "touch reloaded"));
    assert_success(&env.run(&["up", "-d", "--wait"]), "reload changed hook");
    assert!(env.project.join("reloaded").exists());
    let ps = env.ps_json_value();
    assert_ne!(hook_service(&ps, "worker")["pid"], original);
    env.down_json();
}

#[test]
fn startup_hooks_automatic_restart_reevaluates_both_phases() {
    let mut env = TestEnv::new();
    env.with_config(r#"
processes:
  svc:
    command: 'if test -f second; then exec sleep 100; fi; while ! test -f post; do sleep 0.02; done; touch second; exit 1'
    restart_policy: on_failure
    backoff_seconds: 0
    pre_start: [{name: every, command: 'echo pre >> attempts'}]
    post_start: [{name: guarded, creates: post, command: 'echo post >> attempts; touch post'}]
"#);
    env.up_detach_json();
    let ps = wait_hook_snapshot(&env, |ps| {
        let p = hook_service(ps, "svc");
        p["restart_count"] == 1 && p["initialization"]["initialized"] == true
    });
    assert_eq!(
        hook_service(&ps, "svc")["initialization"]["hooks"][1]["reason"],
        "already_satisfied"
    );
    assert_eq!(
        fs::read_to_string(env.project.join("attempts")).unwrap(),
        "pre\npost\npre\n"
    );
    env.down_json();
}

#[test]
fn startup_hooks_one_off_commands_are_excluded_and_no_deps_still_runs_hooks() {
    let mut env = TestEnv::new();
    env.with_config(
        r#"
processes:
  dep:
    command: 'sleep 100'
    pre_start: [{name: dep, command: 'touch dependency'}]
  svc:
    command: 'sleep 100'
    pre_start: [{name: once, command: 'echo hook >> attempts'}]
"#,
    );
    assert_success(&env.run(&["run", "svc", "true"]), "one off run");
    assert!(!env.project.join("attempts").exists());
    env.up_started = true;
    assert_success(&env.run(&["up", "-d", "--no-deps", "svc"]), "no deps");
    wait_hook_snapshot(&env, |ps| {
        hook_service(ps, "svc")["initialization"]["initialized"] == true
    });
    assert!(!env.project.join("dependency").exists());
    assert_success(&env.run(&["exec", "svc", "true"]), "one off exec");
    assert_eq!(
        fs::read_to_string(env.project.join("attempts")).unwrap(),
        "hook\n"
    );
    env.down_json();
}

#[test]
fn startup_hooks_exit_modes_retain_failure_and_ignore_hook_success() {
    for phase in ["pre_start", "post_start"] {
        let mut env = TestEnv::new();
        env.with_config(&format!("exit_mode: exit_on_failure\nprocesses:\n  svc:\n    command: 'sleep 100'\n    {phase}: [{{name: broken, command: 'exit 9'}}]\n"));
        env.up_started = true;
        let out = env.run(&["up", "--json"]);
        assert!(!out.status.success());
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(text.contains("broken"), "{text}");
    }
    let mut env = TestEnv::new();
    env.with_config("exit_mode: exit_on_end\nprocesses:\n  svc:\n    command: 'sleep 100'\n    pre_start: [{name: done, command: 'exit 0'}]\n");
    env.up_detach_json();
    let ps = wait_hook_snapshot(&env, |ps| {
        hook_service(ps, "svc")["initialization"]["initialized"] == true
    });
    assert_eq!(hook_service(&ps, "svc")["state"], "running");
    env.down_json();
}

#[test]
fn startup_hooks_http_wait_is_independent_of_readiness() {
    // Hold the response until the service asks for an admin check. A 302
    // exercises the existing HTTP probe success rule (200..400).
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    listener.set_nonblocking(true).unwrap();
    let mut env = TestEnv::new();
    env.with_config(&format!(
        r#"
processes:
  svc:
    command: 'touch main-started; exec sleep 100'
    readiness_probe: {{exec: {{command: 'test -f initialized'}}, period_seconds: 1}}
    post_start:
      - name: admin
        wait_for: {{http_get: {{port: {port}}}}}
        command: 'touch initialized'
"#
    ));
    env.up_detach_json();
    let ps = wait_hook_snapshot(&env, |ps| {
        hook_service(ps, "svc")["initialization"]["hooks"][0]["stage"] == "waiting"
    });
    assert_eq!(hook_service(&ps, "svc")["ready"], false);
    assert!(!env.project.join("initialized").exists());
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                let _ = stream.write_all(b"HTTP/1.1 302 Found\r\nContent-Length: 0\r\n\r\n");
                break;
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                assert!(std::time::Instant::now() < deadline);
                thread::sleep(Duration::from_millis(10));
            }
            Err(e) => panic!("{e}"),
        }
    }
    assert_success(
        &env.run(&["up", "-d", "--wait"]),
        "HTTP initialization and readiness",
    );
    env.down_json();
}

#[test]
fn startup_hooks_cli_timeout_reports_stage_without_cancelling_daemon() {
    let mut env = TestEnv::new();
    env.with_config(
        r#"
processes:
  svc:
    command: 'sleep 100'
    pre_start:
      - name: slow
        command: 'while ! test -f release; do sleep 0.02; done'
"#,
    );
    env.up_detach_json();
    wait_hook_snapshot(&env, |ps| {
        hook_service(ps, "svc")["initialization"]["hooks"][0]["stage"] == "executing"
    });
    let out = run_cmd(
        &env.project,
        &env.runtime,
        &env.state,
        &env.home,
        &["up", "-d", "--wait"],
        &[("DECOMPOSE_DAEMON_READY_TIMEOUT_MS", "200")],
        &[],
    );
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("pre_start:slow"));
    assert_eq!(
        hook_service(&env.ps_json_value(), "svc")["state"],
        "initializing"
    );
    fs::write(env.project.join("release"), "").unwrap();
    wait_hook_snapshot(&env, |ps| {
        hook_service(ps, "svc")["initialization"]["initialized"] == true
    });
    env.down_json();
}

#[test]
fn startup_hooks_exited_one_shot_uses_historical_success() {
    let mut env = TestEnv::new();
    env.with_config("processes:\n  job:\n    command: 'exit 0'\n    pre_start: [{name: prepare, command: 'touch prepared'}]\n");
    env.up_detach_json();
    let ps = wait_hook_snapshot(&env, |ps| hook_service(ps, "job")["state"] == "exited");
    assert_eq!(
        hook_service(&ps, "job")["initialization"]["state"],
        "succeeded"
    );
    assert_eq!(
        hook_service(&ps, "job")["initialization"]["initialized"],
        false
    );
    assert_success(&env.run(&["up", "-d", "--wait"]), "one-shot initialization");
    env.down_json();
}

#[test]
fn startup_hooks_artifact_shortcuts_and_hook_environment_are_isolated() {
    let mut env = TestEnv::new();
    fs::create_dir(env.project.join("subdir")).unwrap();
    fs::write(env.project.join("subdir/existing"), "").unwrap();
    env.with_config(
        r#"
processes:
  svc:
    command: 'test "$$VALUE" = service; exec sleep 100'
    environment: {VALUE: service}
    pre_start:
      - name: local-shortcut
        working_dir: subdir
        creates: existing
        wait_for: {exec: {command: 'exit 1'}}
        timeout_seconds: 1
        command: 'touch never'
      - name: recheck
        working_dir: subdir
        creates: created-during-wait
        wait_for: {exec: {command: 'touch created-during-wait'}}
        command: 'touch never'
      - name: env
        working_dir: subdir
        environment: ['VALUE=hook']
        command: 'test "$$VALUE" = hook && pwd > location && echo "$$VALUE" > value'
      - name: isolated
        command: 'test "$$VALUE" = service && touch isolated'
"#,
    );
    env.up_detach_json();
    assert_success(
        &env.run(&["up", "-d", "--wait"]),
        "artifact shortcuts and env",
    );
    assert_eq!(
        fs::read_to_string(env.project.join("subdir/value")).unwrap(),
        "hook\n"
    );
    assert!(env.project.join("isolated").exists());
    assert!(!env.project.join("subdir/never").exists());
    let ps = env.ps_json_value();
    for idx in [0, 1] {
        assert_eq!(
            hook_service(&ps, "svc")["initialization"]["hooks"][idx]["reason"],
            "already_satisfied"
        );
    }
    let effective = env.run(&["config", "--json"]);
    assert_success(&effective, "config");
    let cfg: Value = serde_json::from_slice(&effective.stdout).unwrap();
    assert!(
        cfg["processes"]["svc"]["pre_start"][0]["creates"]
            .as_str()
            .unwrap()
            .ends_with("subdir/existing")
    );
    assert_eq!(cfg["processes"]["svc"]["post_start"], serde_json::json!([]));
    env.down_json();
}

#[test]
fn startup_hooks_liveness_restart_cancels_previous_incarnation() {
    let mut env = TestEnv::new();
    env.with_config(r#"
disable_env_expansion: true
processes:
  svc:
    command: 'sleep 100'
    restart_policy: always
    max_restarts: 1
    backoff_seconds: 0
    liveness_probe:
      exec: {command: 'test -f second'}
      period_seconds: 1
      initial_delay_seconds: 1
      failure_threshold: 1
    post_start:
      - name: blocked-first-time
        command: 'if test -f hook.pid; then touch second; else echo $$ > hook.pid; exec sleep 100; fi'
"#);
    env.up_detach_json();
    let ps = wait_hook_snapshot(&env, |ps| {
        let p = hook_service(ps, "svc");
        p["restart_count"] == 1 && p["initialization"]["initialized"] == true
    });
    assert_eq!(
        hook_service(&ps, "svc")["initialization"]["hooks"][0]["status"],
        "succeeded"
    );
    let pid = fs::read_to_string(env.project.join("hook.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(!process_alive(pid));
    env.down_json();
}

#[test]
fn startup_hooks_documented_example_runs_and_skips_on_restart() {
    let mut env = TestEnv::new();
    env.with_config(include_str!("../examples/startup-hooks.yml"));
    env.up_detach_json();
    assert_success(&env.run(&["up", "-d", "--wait"]), "documented example");
    assert_success(&env.run(&["restart", "database"]), "example restart");
    let ps = wait_hook_snapshot(&env, |ps| {
        hook_service(ps, "database")["initialization"]["initialized"] == true
    });
    for h in hook_service(&ps, "database")["initialization"]["hooks"]
        .as_array()
        .unwrap()
    {
        assert_eq!(h["reason"], "already_satisfied");
    }
    env.down_json();
}

#[test]
fn startup_hooks_nonterminating_signal_preserves_completed_initialization() {
    let mut env = TestEnv::new();
    env.with_config(
        r#"
processes:
  svc:
    command: "sleep 100"
    post_start:
      - name: prepare
        command: "true"
  dependent:
    command: "sleep 100"
    depends_on:
      svc: {condition: process_initialized}
"#,
    );
    env.up_started = true;
    assert_success(&env.run(&["up", "-d", "--wait", "svc"]), "start service");
    let before = env.ps_json_value();
    assert_eq!(
        hook_service(&before, "svc")["initialization"]["initialized"],
        true
    );
    assert_eq!(hook_service(&before, "dependent")["state"], "not_started");

    assert_success(
        &env.run(&["kill", "--signal", "CONT", "svc"]),
        "send nonterminating signal",
    );
    let after = env.ps_json_value();
    assert_eq!(hook_service(&after, "svc")["state"], "running");
    assert_eq!(
        hook_service(&after, "svc")["pid"],
        hook_service(&before, "svc")["pid"]
    );
    assert_eq!(
        hook_service(&after, "svc")["initialization"],
        hook_service(&before, "svc")["initialization"]
    );

    assert_success(&env.run(&["start", "dependent"]), "start dependent");
    wait_hook_snapshot(&env, |ps| {
        hook_service(ps, "dependent")["state"] == "running"
    });
    env.down_json();
}

#[test]
fn startup_hooks_kill_cancels_work_even_if_main_ignores_signal() {
    let mut env = TestEnv::new();
    env.with_config(
        r#"
disable_env_expansion: true
processes:
  svc:
    command: "trap '' TERM; touch main-ready; while :; do sleep 1; done"
    shutdown: {timeout_seconds: 1}
    post_start:
      - name: active
        wait_for: {exec: {command: 'test -f main-ready'}}
        command: 'echo $$ > hook.pid; exec sleep 100'
"#,
    );
    env.up_detach_json();
    wait_hook_snapshot(&env, |_| env.project.join("hook.pid").exists());
    let pid = fs::read_to_string(env.project.join("hook.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_success(
        &env.run(&["kill", "--signal", "TERM", "svc"]),
        "signal service",
    );
    let ps = wait_hook_snapshot(&env, |ps| {
        hook_service(ps, "svc")["initialization"]["state"] == "cancelled"
    });
    assert_eq!(hook_service(&ps, "svc")["state"], "running");
    assert!(!process_alive(pid));
    env.down_json();
}

#[test]
fn startup_hooks_reap_background_descendants_after_shell_success() {
    let mut env = TestEnv::new();
    env.with_config(
        r#"
disable_env_expansion: true
processes:
  svc:
    command: sleep 100
    pre_start:
      - name: descendants
        command: 'sleep 100 & echo $! > child.pid; exit 0'
"#,
    );
    env.up_detach_json();
    wait_hook_snapshot(&env, |ps| {
        hook_service(ps, "svc")["initialization"]["initialized"] == true
    });
    let pid = fs::read_to_string(env.project.join("child.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(!process_alive(pid));
    env.down_json();
}

#[test]
fn startup_hooks_explicit_retry_discards_previous_failure_for_waiting() {
    let mut env = TestEnv::new();
    env.with_config(
        r#"
processes:
  svc:
    command: sleep 100
    pre_start:
      - name: recoverable
        command: 'test -f fixed'
"#,
    );
    env.up_detach_json();
    wait_hook_snapshot(&env, |ps| {
        hook_service(ps, "svc")["state"] == "failed_to_start"
    });
    fs::write(env.project.join("fixed"), "").unwrap();
    assert_success(
        &env.run(&["up", "-d", "--wait"]),
        "explicit retry of failed hook",
    );
    assert_eq!(
        hook_service(&env.ps_json_value(), "svc")["initialization"]["state"],
        "succeeded"
    );
    env.down_json();
}

#[test]
fn included_fragment_config_run_and_reload_share_resolution_and_identity() {
    let mut env = TestEnv::new();
    let fragments = env._root.path().join("fragments");
    fs::create_dir_all(&fragments).unwrap();
    fs::create_dir_all(env.project.join("work")).unwrap();
    fs::write(fragments.join(".env"), "FRAGMENT_ONLY=must-not-load\n").unwrap();
    fs::write(fragments.join("service.env"), "FROM_FRAGMENT=asset\n").unwrap();
    fs::write(
        env.project.join(".env"),
        "DECOMPOSE_FRAGMENT_PATH=../fragments/service.yaml\nROOT_ONLY=root\n",
    )
    .unwrap();
    let fragment = fragments.join("service.yaml");
    let definition = |version: &str| {
        format!(
            r#"
processes:
  api:
    command: 'exec /bin/sleep 30'
    working_dir: work
    env_file: ['${{DECOMPOSE_FILE_DIR}}/service.env']
    environment:
      VERSION: '{version}'
      PORT: '3000'
      ASSET_DIR: '${{DECOMPOSE_FILE_DIR}}'
  excluded:
    command: /bin/false
"#
        )
    };
    fs::write(&fragment, definition("first")).unwrap();
    env.with_config(
        r#"
include:
  - path: '${DECOMPOSE_FRAGMENT_PATH}'
    processes: [api]
processes:
  api:
    environment: {PORT: '9000'}
"#,
    );
    let output = env.run(&["config", "--json"]);
    assert_success(&output, "config includes");
    let config: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(config["processes"].as_object().unwrap().len(), 1);
    assert_eq!(config["processes"]["api"]["environment"]["PORT"], "9000");
    assert_eq!(
        config["provenance"]["processes"]["api"]["command_source"],
        fragment.canonicalize().unwrap().to_str().unwrap()
    );
    assert_eq!(
        config["provenance"]["processes"]["api"]["files"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let output = env.run(&["run", "api", "/bin/sh", "-c", "printf '%s|%s|%s|%s|%s' \"$PORT\" \"$ROOT_ONLY\" \"$FROM_FRAGMENT\" \"${FRAGMENT_ONLY:-absent}\" \"$PWD\""]);
    assert_success(&output, "run included api");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!(
            "9000|root|asset|absent|{}",
            env.project.join("work").canonicalize().unwrap().display()
        )
    );

    let first: Value = serde_json::from_slice(&env.up_detach_json().stdout).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let first_pid = loop {
        if let Some(pid) = pid_of(&env.ps_json_value(), "api") {
            break pid;
        }
        assert!(std::time::Instant::now() < deadline, "api never started");
        thread::sleep(Duration::from_millis(30));
    };
    // Moving the source changes provenance but not the effective process.
    fs::rename(&fragment, fragments.join("moved.yaml")).unwrap();
    // Keep dotenv stable: discovery variables also enter the child env.
    env.with_config("include: [{path: ../fragments/moved.yaml, processes: [api]}]\nprocesses: {api: {environment: {PORT: '9000'}}}");
    assert_success(&env.run(&["up", "-d", "--json"]), "reload moved fragment");
    assert_eq!(pid_of(&env.ps_json_value(), "api"), Some(first_pid));

    fs::write(fragments.join("moved.yaml"), definition("second")).unwrap();
    let output = env.run(&["up", "-d", "--json"]);
    assert_success(&output, "reload changed fragment");
    let second = serde_json::Deserializer::from_slice(&output.stdout)
        .into_iter::<Value>()
        .map(Result::unwrap)
        .find(|record| record.get("daemon").is_some())
        .expect("up status record");
    assert!(first["daemon"]["pid"].is_u64());
    assert_eq!(
        first["daemon"]["pid"], second["daemon"]["pid"],
        "same daemon handles include edits"
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(pid) = pid_of(&env.ps_json_value(), "api")
            && pid != first_pid
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "fragment edit did not restart api"
        );
        thread::sleep(Duration::from_millis(30));
    }
    let output = env.run(&["exec", "api", "/bin/sh", "-c", "printf '%s' \"$VERSION\""]);
    assert_success(&output, "exec included api after reload");
    assert_eq!(output.stdout, b"second");
}

#[test]
fn include_preflight_reports_conflicts_without_starting_daemon() {
    let mut env = TestEnv::new();
    fs::write(
        env.project.join("a.yaml"),
        "processes: {api: {command: /bin/sleep 30}}",
    )
    .unwrap();
    fs::write(
        env.project.join("b.yaml"),
        "processes: {api: {command: /bin/sleep 60}}",
    )
    .unwrap();
    env.with_config("include: [a.yaml, b.yaml]");
    let output = env.run(&["up", "-d", "--json"]);
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("conflicts") && error.contains("a.yaml") && error.contains("b.yaml"));
    assert!(!error.contains("daemon did not become ready"));
}

#[test]
fn included_hooks_keep_source_anchors_and_resolve_environment_once() {
    let mut env = TestEnv::new();
    let fragments = env._root.path().join("hook-fragments");
    fs::create_dir_all(&fragments).unwrap();
    fs::create_dir_all(env.project.join("work")).unwrap();
    fs::write(fragments.join("service.env"), "HOOK_VALUE=from-file\n").unwrap();
    fs::write(
        fragments.join("prepare.sh"),
        "test \"$HOOK_VALUE\" = from-file && printf '%s' \"$HOOK_ASSET\" > prepared\n",
    )
    .unwrap();
    fs::write(
        fragments.join("service.yaml"),
        r#"
processes:
  svc:
    command: sleep 100
    working_dir: work
    env_file: ['${DECOMPOSE_FILE_DIR}/service.env']
    pre_start:
      - name: prepare
        creates: prepared
        environment: {HOOK_ASSET: '${DECOMPOSE_FILE_DIR}'}
        command: 'sh "${DECOMPOSE_FILE_DIR}/prepare.sh"'
    post_start:
      - name: replaced
        command: 'exit 1'
"#,
    )
    .unwrap();
    env.with_config(
        r#"
include: [../hook-fragments/service.yaml]
processes:
  svc:
    post_start:
      - name: local
        command: |
          set -eu
          value=inline
          test "$$value" = inline
          test '${HOOK_VALUE}' = from-file
          test '${DECOMPOSE_PROJECT_DIR}' = '${DECOMPOSE_FILE_DIR}'
          touch '${DECOMPOSE_FILE_DIR}/local-marker'
"#,
    );
    let output = env.run(&["config", "--json"]);
    assert_success(&output, "resolve included hooks");
    let config: Value = serde_json::from_slice(&output.stdout).unwrap();
    let pre = &config["processes"]["svc"]["pre_start"][0];
    assert_eq!(
        pre["creates"],
        env.project
            .canonicalize()
            .unwrap()
            .join("work/prepared")
            .to_str()
            .unwrap()
    );
    assert!(pre.get("interpolation_anchors").is_none());
    env.up_started = true;
    assert_success(&env.run(&["up", "-d", "--wait"]), "run included hooks");
    assert_eq!(
        fs::read_to_string(env.project.join("work/prepared")).unwrap(),
        fragments.canonicalize().unwrap().to_str().unwrap()
    );
    assert!(env.project.join("local-marker").exists());
    env.down_json();
}

#[test]
fn output_flags_are_global_and_parse_errors_respect_child_boundaries() {
    let env = TestEnv::new();
    for args in [vec!["--json", "ps"], vec!["ps", "--json"]] {
        let output = env.run(&args);
        assert_success(&output, "global json");
        let result: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(result["schema_version"], "1.0");
        assert_eq!(result["daemon"]["state"], "not_running");
    }
    for args in [
        vec!["ps", "--json", "--table"],
        vec!["--json", "up", "-d", "--wait", "--no-start"],
        vec!["tui", "--json"],
        vec!["--json", "up", "--tui"],
    ] {
        let output = env.run(&args);
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        let error: Value = serde_json::from_slice(&output.stderr).unwrap();
        assert_eq!(error["code"], "invalid_arguments");
    }
    for args in [
        vec!["--session=--json", "bad-command"],
        vec!["run", "missing", "echo", "--json"],
    ] {
        let output = env.run(&args);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).starts_with("error:"));
    }
    let help = env.run(&["--json", "--help"]);
    assert_success(&help, "help");
    assert!(help.stderr.is_empty());
    assert!(String::from_utf8_lossy(&help.stdout).contains("Usage:"));
    let child = env.run(&["--json", "run", "sleeper", "printf", "%s", "--json"]);
    assert_success(&child, "child passthrough");
    assert_eq!(child.stdout, b"--json");
}

#[test]
fn fresh_no_start_and_empty_wait_have_truthful_results() {
    let mut env = TestEnv::new();
    let up = env.run(&["up", "-d", "--no-start", "--json"]);
    assert_success(&up, "fresh no-start");
    env.up_started = true;
    let result: Value = serde_json::from_slice(&up.stdout).unwrap();
    assert_eq!(result["services"][0]["outcome"], "registered");
    assert_eq!(env.ps_json_value()["processes"][0]["state"], "not_started");
    let text = env.run(&["ps"]);
    assert!(String::from_utf8_lossy(&text.stdout).contains("daemon running; no processes started"));
    env.down_json();
    env.with_config("processes:\n  disabled:\n    command: sleep 300\n    disabled: true\n");
    let up = env.run(&["up", "-d", "--wait", "--json"]);
    assert_success(&up, "empty eligible wait");
    env.up_started = true;
    let result: Value = serde_json::from_slice(&up.stdout).unwrap();
    assert_eq!(result["services"], serde_json::json!([]));
    assert_eq!(result["readiness"], "satisfied");
    env.down_json();
}

#[test]
fn json_logs_preserve_streams_and_validation_warnings_are_diagnostics() {
    let mut env = TestEnv::new();
    env.with_config("processes:\n  source:\n    command: \"printf 'Mixed Case\\\\n'; printf 'Error Data\\\\n' >&2\"\n");
    let up = env.run(&["up", "-d", "--wait", "--json"]);
    assert_success(&up, "one-shot logs");
    env.up_started = true;
    let output = env.run(&["logs", "--json"]);
    assert_success(&output, "json logs");
    let records: Vec<Value> = serde_json::Deserializer::from_slice(&output.stdout)
        .into_iter()
        .map(Result::unwrap)
        .collect();
    assert!(
        records
            .iter()
            .any(|r| r["type"] == "log" && r["stream"] == "stdout" && r["message"] == "Mixed Case")
    );
    assert!(
        records
            .iter()
            .any(|r| r["type"] == "log" && r["stream"] == "stderr" && r["message"] == "Error Data")
    );
    assert!(records.iter().all(|r| r["schema_version"] == "1.0"));
    assert!(output.stderr.is_empty());
    let empty = env.run(&["logs", "--json", "nonexistent"]);
    assert_success(&empty, "empty logs");
    assert!(empty.stdout.is_empty());
    assert!(empty.stderr.is_empty());
    env.down_json();

    env.with_config("processes:\n  test:\n    command: sleep 300\n    readiness_probe:\n      exec: {command: 'true'}\n      period_seconds: 1\n      timeout_seconds: 1\n");
    let output = env.run(&["config", "--json"]);
    assert_success(&output, "warning with result");
    let _: Value = serde_json::from_slice(&output.stdout).unwrap();
    let warning: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(warning["severity"], "warning");
    assert_eq!(warning["code"], "probe_timing_no_slack");
}
