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
      period_seconds: 2
      timeout_seconds: 1
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
        if let Ok(ps_json) = serde_json::from_slice::<Value>(&ps.stdout) {
            if let Some(server) = ps_json["processes"]
                .as_array()
                .and_then(|a| a.iter().find(|p| p["name"].as_str() == Some("server")))
            {
                if server["healthy"].as_bool() == Some(true) {
                    assert_eq!(
                        server["has_readiness_probe"].as_bool(),
                        Some(true),
                        "has_readiness_probe should be true"
                    );
                    healthy = true;
                    break;
                }
            }
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
// Config-reload integration tests (bd decompose-rn2)
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
    let a_json: Value = serde_json::from_slice(&out_a.stdout).expect("a json");
    let b_json: Value = serde_json::from_slice(&out_b.stdout).expect("b json");
    let pid_a = a_json.get("pid").and_then(Value::as_u64);
    let pid_b = b_json.get("pid").and_then(Value::as_u64);
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
#[ignore = "process-group handling not yet implemented — see bd issue"]
fn shutdown_terminates_grandchild_processes() {
    // A shell command that forks off a long-lived grandchild. On `down`
    // the daemon should signal the whole process group so the grandchild
    // dies with its parent — otherwise we leak a `sleep` for the caller
    // to notice later.
    //
    // Currently the daemon sends SIGTERM to the direct child pid only, so
    // backgrounded grandchildren are orphaned. This test is `#[ignore]`d
    // until the daemon spawns children into their own process group (via
    // `setsid`/`pre_exec`) and signals the group on shutdown. Runs with
    // `cargo test -- --ignored` for local verification once fixed.
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

    // Sanity: the grandchild is alive right now (kill -0 returns 0).
    let alive_before = Command::new("kill")
        .arg("-0")
        .arg(child_pid.to_string())
        .status()
        .expect("kill -0");
    assert!(
        alive_before.success(),
        "grandchild pid {child_pid} should be alive before down"
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
        let status = Command::new("kill")
            .arg("-0")
            .arg(child_pid.to_string())
            .status()
            .expect("kill -0 after down");
        if !status.success() {
            still_alive = false;
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    if still_alive {
        // Best effort cleanup so the grandchild doesn't outlive the test
        // binary even when we fail.
        let _ = Command::new("kill")
            .arg("-KILL")
            .arg(child_pid.to_string())
            .status();
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
    match parsed.get("running") {
        Some(Value::Bool(b)) => *b,
        _ => parsed.get("processes").is_some(),
    }
}

/// Observe daemon liveness without generating IPC traffic. Every IPC
/// request resets the orphan-watchdog clock, so the auto-exit tests need a
/// zero-touch probe — we read the PID file the daemon writes at startup
/// and send `kill(pid, 0)` to check whether the process is still alive.
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
        // `kill -0` on Unix: exit 0 = alive (or permission denied),
        // non-zero = ESRCH or similar. We want "alive".
        let status = Command::new("kill").arg("-0").arg(pid.to_string()).status();
        if let Ok(s) = status
            && s.success()
        {
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
    // should arrive, and the watchdog should trip. Note: from here on we
    // must NOT issue IPC against the daemon, because every request resets
    // the orphan activity clock and defeats the test.
    let kill_status = Command::new("kill")
        .arg("-KILL")
        .arg(child.id().to_string())
        .status()
        .expect("send sigkill");
    assert!(kill_status.success(), "failed to SIGKILL up");
    let _ = child.wait();

    // Grace is 2s, watchdog tick is 1s. Allow generous slack.
    let exited = wait_for_daemon_exit_no_ipc(&state, Duration::from_secs(15));
    assert!(
        exited,
        "daemon should self-exit after orphan grace period elapsed",
    );
}

#[test]
fn client_activity_keeps_orphaned_daemon_alive() {
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
    let kill_status = Command::new("kill")
        .arg("-KILL")
        .arg(child.id().to_string())
        .status()
        .expect("send sigkill");
    assert!(kill_status.success(), "failed to SIGKILL up");
    let _ = child.wait();

    // For ~8 seconds (well past the 3s grace), keep hitting the daemon at
    // 500ms intervals. Each request should reset the activity clock, so
    // the daemon must still be alive at the end. We use the IPC probe
    // here deliberately — the whole point is that IPC activity keeps the
    // daemon alive.
    let hold_start = std::time::Instant::now();
    while hold_start.elapsed() < Duration::from_secs(8) {
        assert!(
            is_daemon_live_ipc(&project, &runtime, &state, &home, &cfg),
            "daemon exited while IPC activity was ongoing at {:?}",
            hold_start.elapsed(),
        );
        thread::sleep(Duration::from_millis(500));
    }

    // Stop poking it. Switch to the no-IPC probe so we don't reset the
    // watchdog clock while waiting for it to fire.
    let exited = wait_for_daemon_exit_no_ipc(&state, Duration::from_secs(15));
    assert!(
        exited,
        "daemon should self-exit after IPC activity stops and grace elapses",
    );
}
