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
        &["up", "-f", &cfg, "--detach", "--json"],
        &[],
        &[],
    );
    assert_success(&up, "up --json");
    let up_json: Value = serde_json::from_slice(&up.stdout).expect("up json");
    assert_eq!(up_json.get("status").and_then(Value::as_str), Some("started"));

    let ps_json = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["ps", "-f", &cfg, "--json"],
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
        &["ps", "-f", &cfg, "--table"],
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
        &["down", "-f", &cfg, "--json"],
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
        &["up", "-f", &cfg, "--detach", "--json"],
        &[],
        &[],
    );
    assert_success(&up, "up");

    let ps_default_table = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["ps", "-f", &cfg],
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
        &["ps", "-f", &cfg],
        &[],
        &["CI", "LLM"],
    );
    assert_success(&ps_default_json, "default json ps");
    let parsed: Value = serde_json::from_slice(&ps_default_json.stdout).expect("default json output");
    assert!(parsed.get("processes").and_then(Value::as_array).is_some());

    let down = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["down", "-f", &cfg, "--json"],
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
        .arg("up")
        .arg("-f")
        .arg(&cfg)
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
        &["ps", "-f", &cfg, "--json"],
        &[],
        &[],
    );
    assert_success(&ps, "ps after ctrl-c detach");

    let down = run_cmd(
        &project,
        &runtime,
        &state,
        &home,
        &["down", "-f", &cfg, "--json"],
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
        &["up", "-f", &cfg, "--detach", "--json"],
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
        &["stop", "-f", &cfg, "--json", "alpha"],
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
        &["stop", "-f", &cfg, "--json"],
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
        &["stop", "-f", &cfg, "--json", "no-such-service"],
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
        &["down", "-f", &cfg, "--json"],
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
        &["down", "-f", &cfg, "--json"],
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
        &["ps", "-f", &cfg, "--json"],
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
        &["ps", "-f", &cfg, "--table"],
        &[],
        &[],
    );
    assert_success(&ps_table, "ps --table when not running");
    let table = String::from_utf8_lossy(&ps_table.stdout);
    assert!(table.contains("No processes running"));
}
