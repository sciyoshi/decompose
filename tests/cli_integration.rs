#![cfg(not(windows))]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
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
fn cli_supports_json_and_table_modes() {
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
    let up_json: Value = serde_json::from_slice(&up.stdout).expect("up json");
    assert_eq!(
        up_json.get("status").and_then(Value::as_str),
        Some("started")
    );

    let ps_json = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "ps", "--json"],
        &[],
        &[],
    );
    assert_success(&ps_json, "ps --json");
    let parsed: Value = serde_json::from_slice(&ps_json.stdout).expect("ps json");
    assert!(parsed.get("processes").and_then(Value::as_array).is_some());

    let ps_table = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "ps", "--table"],
        &[],
        &[],
    );
    assert_success(&ps_table, "ps --table");
    let ps_table_text = String::from_utf8_lossy(&ps_table.stdout);
    assert!(ps_table_text.contains("NAME"));
    assert!(ps_table_text.contains("sleeper"));

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
    let down_json: Value = serde_json::from_slice(&down.stdout).expect("down json");
    assert_eq!(down_json.get("status").and_then(Value::as_str), Some("ok"));
}

#[test]
fn default_output_mode_uses_ci_or_llm_table_else_json() {
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
    assert!(table_text.contains("NAME"));

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
    let parsed: Value =
        serde_json::from_slice(&ps_default_json.stdout).expect("default json output");
    assert!(parsed.get("processes").and_then(Value::as_array).is_some());

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
fn ctrl_c_detaches_and_daemon_keeps_running() {
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

    let status = Command::new("kill")
        .arg("-INT")
        .arg(child.id().to_string())
        .status()
        .expect("send ctrl-c");
    assert!(status.success(), "failed to send SIGINT");

    let up_exit = child.wait().expect("wait up");
    assert!(up_exit.success(), "up should detach cleanly");

    let ps = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "ps", "--json"],
        &[],
        &[],
    );
    assert_success(&ps, "ps after ctrl-c detach");

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
    assert_eq!(stop_json.get("status").and_then(Value::as_str), Some("ok"));

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
    let (_root, project, runtime, state, config) = setup_project();
    let home = project.parent().expect("parent").join("home");
    let cfg = config.to_string_lossy().to_string();

    let down = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "down", "--json"],
        &[],
        &[],
    );
    assert_success(&down, "down when nothing is running");
    let parsed: Value = serde_json::from_slice(&down.stdout).expect("down json");
    assert_eq!(parsed.get("status").and_then(Value::as_str), Some("ok"));
}

#[test]
fn ps_when_not_running_is_empty_not_error() {
    let (_root, project, runtime, state, config) = setup_project();
    let home = project.parent().expect("parent").join("home");
    let cfg = config.to_string_lossy().to_string();

    let ps_json = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "ps", "--json"],
        &[],
        &[],
    );
    assert_success(&ps_json, "ps --json when not running");
    let parsed: Value = serde_json::from_slice(&ps_json.stdout).expect("json parse");
    assert_eq!(parsed.get("running").and_then(Value::as_bool), Some(false));
    assert_eq!(
        parsed
            .get("processes")
            .and_then(Value::as_array)
            .map(std::vec::Vec::len),
        Some(0)
    );

    let ps_table = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "ps", "--table"],
        &[],
        &[],
    );
    assert_success(&ps_table, "ps --table when not running");
    let table = String::from_utf8_lossy(&ps_table.stdout);
    assert!(table.contains("No processes running"));
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
    command: "node server.js"
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
    assert_eq!(kill_json.get("status").and_then(Value::as_str), Some("ok"));

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
        envs[0].get("status").and_then(Value::as_str),
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
    assert!(table_text.contains("NAME"));
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
    assert_eq!(down_json.get("status").and_then(Value::as_str), Some("ok"));
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
    let (_root, project, runtime, state, config) = setup_project();
    let home = project.parent().expect("parent").join("home");
    let cfg = config.to_string_lossy().to_string();

    let up = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "up", "-d", "--wait", "--json"],
        &[],
        &[],
    );
    assert_success(&up, "up -d --wait");

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
fn shutdown_normal_sigterm_clean_exit() {
    let (_root, project, runtime, state, _config) = setup_project();
    let home = project.parent().expect("parent").join("home");

    let cfg_path = project.join("decompose.yaml");
    fs::write(
        &cfg_path,
        r#"
processes:
  trapper:
    command: "sh -c 'trap \"exit 0\" TERM; sleep 30'"
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

    // Give the process time to start and register the trap.
    thread::sleep(Duration::from_millis(500));

    let down = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "down", "--json"],
        &[],
        &[],
    );
    assert_success(&down, "down after SIGTERM clean exit");
    let down_json: Value = serde_json::from_slice(&down.stdout).expect("down json");
    assert_eq!(down_json.get("status").and_then(Value::as_str), Some("ok"));
}

#[test]
fn shutdown_timeout_escalation_to_sigkill() {
    let (_root, project, runtime, state, _config) = setup_project();
    let home = project.parent().expect("parent").join("home");

    // Process that ignores SIGTERM, so shutdown must escalate to SIGKILL.
    let cfg_path = project.join("decompose.yaml");
    fs::write(
        &cfg_path,
        r#"
processes:
  stubborn:
    command: "sh -c 'trap \"\" TERM; sleep 30'"
    shutdown:
      timeout_seconds: 1
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

    // Give the process time to start and register the trap.
    thread::sleep(Duration::from_millis(500));

    let start = std::time::Instant::now();
    let down = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "down", "--timeout", "1", "--json"],
        &[],
        &[],
    );
    let elapsed = start.elapsed();

    assert_success(&down, "down after timeout escalation to SIGKILL");
    let down_json: Value = serde_json::from_slice(&down.stdout).expect("down json");
    assert_eq!(down_json.get("status").and_then(Value::as_str), Some("ok"));

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
    let (_root, project, runtime, state, _config) = setup_project();
    let home = project.parent().expect("parent").join("home");

    // Process that traps SIGINT (signal 2) and exits cleanly, but ignores SIGTERM.
    let cfg_path = project.join("decompose.yaml");
    fs::write(
        &cfg_path,
        r#"
processes:
  custom_sig:
    command: "sh -c 'trap \"exit 0\" INT; trap \"\" TERM; sleep 30'"
    shutdown:
      signal: 2
      timeout_seconds: 5
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

    // Give the process time to start and register the traps.
    thread::sleep(Duration::from_millis(500));

    let start = std::time::Instant::now();
    let down = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["--file", &cfg, "down", "--json"],
        &[],
        &[],
    );
    let elapsed = start.elapsed();

    assert_success(&down, "down with custom SIGINT shutdown signal");
    let down_json: Value = serde_json::from_slice(&down.stdout).expect("down json");
    assert_eq!(down_json.get("status").and_then(Value::as_str), Some("ok"));

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
        ps_def_json.get("running").and_then(Value::as_bool),
        Some(false),
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
            obj.get("healthy").and_then(Value::as_bool).is_some(),
            "process must have bool 'healthy', got: {proc}"
        );
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
        .get("status")
        .and_then(Value::as_str)
        .expect("up response must have string 'status'");
    assert_eq!(status, "started");

    // Must have "pid" numeric field.
    assert!(
        parsed.get("pid").and_then(Value::as_u64).is_some(),
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
        .get("status")
        .and_then(Value::as_str)
        .expect("down response must have string 'status'");
    assert_eq!(status, "ok");
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

    // Must have "running" boolean field set to false.
    let running = parsed
        .get("running")
        .and_then(Value::as_bool)
        .expect("empty ps response must have bool 'running'");
    assert!(!running, "running should be false when no daemon");

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
        start_json.get("status").and_then(Value::as_str),
        Some("ok"),
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
    let (_root, project, runtime, state, _config) = setup_project();
    let home = project.parent().expect("parent").join("home");

    // The marker file starts absent; the probe checks for it.
    let marker = project.join("healthy_marker");
    let marker_str = marker.to_string_lossy().to_string();

    let cfg_path = project.join("decompose.yaml");
    fs::write(
        &cfg_path,
        format!(
            r#"
processes:
  web:
    command: "sleep 60"
    readiness_probe:
      exec:
        command: "test -f {marker_str}"
      period_seconds: 1
      timeout_seconds: 2
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

    // Wait for a couple of probe periods — healthy should still be false
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
    let web1 = ps1_json["processes"]
        .as_array()
        .expect("processes array")
        .iter()
        .find(|p| p["name"].as_str() == Some("web"))
        .expect("web process");
    assert_eq!(
        web1["healthy"].as_bool(),
        Some(false),
        "healthy should be false before marker exists"
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
    let web2 = ps2_json["processes"]
        .as_array()
        .expect("processes array")
        .iter()
        .find(|p| p["name"].as_str() == Some("web"))
        .expect("web process");
    assert_eq!(
        web2["healthy"].as_bool(),
        Some(true),
        "healthy should be true after marker is created"
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
fn http_get_readiness_probe_flips_healthy_flag() {
    let (_root, project, runtime, state, _config) = setup_project();
    let home = project.parent().expect("parent").join("home");

    // Use a simple HTTP server via Python
    let cfg_path = project.join("decompose.yaml");
    fs::write(
        &cfg_path,
        r#"
processes:
  server:
    command: "python3 -m http.server 18931"
    readiness_probe:
      http_get:
        host: "127.0.0.1"
        port: 18931
        path: "/"
      period_seconds: 1
      timeout_seconds: 2
      success_threshold: 1
      failure_threshold: 1
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

    // Wait for server to start and probe to detect it
    thread::sleep(Duration::from_secs(5));

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
    let server = ps_json["processes"]
        .as_array()
        .expect("processes array")
        .iter()
        .find(|p| p["name"].as_str() == Some("server"))
        .expect("server process");
    assert_eq!(
        server["healthy"].as_bool(),
        Some(true),
        "healthy should be true after HTTP server starts responding"
    );
    assert_eq!(
        server["has_readiness_probe"].as_bool(),
        Some(true),
        "has_readiness_probe should be true"
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
      period_seconds: 1
      timeout_seconds: 2
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
      period_seconds: 1
      timeout_seconds: 2
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
      period_seconds: 1
      timeout_seconds: 2
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
        svc1["healthy"].as_bool(),
        Some(true),
        "healthy should be true before restart"
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
        svc2["healthy"].as_bool(),
        Some(false),
        "healthy should be false after restart when marker is gone"
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
