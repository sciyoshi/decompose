#![cfg(unix)]

mod support;
use support::process_alive as alive;

use std::fs;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use serde_json::Value;

struct Env {
    root: tempfile::TempDir,
    daemon: Option<u32>,
}

impl Env {
    fn new(config: &str) -> Self {
        let root = tempfile::tempdir().unwrap();
        for dir in ["runtime", "state", "home"] {
            fs::create_dir(root.path().join(dir)).unwrap();
        }
        fs::write(root.path().join("decompose.yaml"), config).unwrap();
        Self { root, daemon: None }
    }
    fn path(&self, file: &str) -> PathBuf {
        self.root.path().join(file)
    }
    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_decompose"));
        cmd.current_dir(self.root.path())
            .env("XDG_RUNTIME_DIR", self.path("runtime"))
            .env("XDG_STATE_HOME", self.path("state"))
            .env("HOME", self.path("home"))
            .args(args);
        cmd
    }
    fn run(&self, args: &[&str]) -> Output {
        let out = self.command(args).output().unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        out
    }
    fn up(&mut self) {
        let out = self.run(&["up", "-d", "--json"]);
        self.daemon = Some(
            serde_json::from_slice::<Value>(&out.stdout).unwrap()["pid"]
                .as_u64()
                .unwrap() as u32,
        );
    }
    fn pidfile(&self, name: &str) -> u32 {
        wait(|| fs::read_to_string(self.path(name)).is_ok_and(|s| s.trim().parse::<u32>().is_ok()));
        fs::read_to_string(self.path(name))
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }
    fn state(&self) -> String {
        let out = self.run(&["ps", "--json"]);
        serde_json::from_slice::<Value>(&out.stdout).unwrap()["processes"][0]["state"]
            .as_str()
            .unwrap_or("")
            .into()
    }
    fn foreground(&mut self) -> Child {
        let child = self
            .command(&["up", "--json"])
            .process_group(0)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        wait(|| {
            self.path("state/decompose").exists()
                && fs::read_dir(self.path("state/decompose"))
                    .unwrap()
                    .flatten()
                    .any(|e| e.path().extension().is_some_and(|e| e == "pid"))
        });
        let pid_path = fs::read_dir(self.path("state/decompose"))
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .find(|p| p.extension().is_some_and(|e| e == "pid"))
            .unwrap();
        self.daemon = Some(
            fs::read_to_string(pid_path)
                .unwrap()
                .trim()
                .parse()
                .unwrap(),
        );
        child
    }
}
impl Drop for Env {
    fn drop(&mut self) {
        let _ = self
            .command(&["down", "--timeout", "0"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if let Some(pid) = self.daemon {
            let _ = kill(Pid::from_raw(pid as i32), Signal::SIGKILL);
        }
        for file in ["worker.pid", "leader.pid", "hook.pid", "probe.pid"] {
            if let Ok(pid) = fs::read_to_string(self.path(file))
                && let Ok(pid) = pid.trim().parse::<i32>()
            {
                let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
            }
        }
    }
}
fn wait(mut predicate: impl FnMut() -> bool) {
    let start = Instant::now();
    while !predicate() {
        assert!(
            start.elapsed() < Duration::from_secs(15),
            "condition timed out"
        );
        sleep(Duration::from_millis(30));
    }
}
fn signal(pid: u32, sig: Signal) {
    kill(Pid::from_raw(pid as i32), sig).unwrap();
}
const CONFIG: &str = "processes:\n  app:\n    command: exec sh service.sh\n    shutdown:\n      timeout_seconds: 1\n";
const FORKER: &str = "trap 'exit 0' TERM\nsh -c 'trap \"\" TERM; echo $$ > worker.pid; exec sleep 120' &\necho $$ > leader.pid\nwait\n";

#[test]
fn stop_waits_for_term_ignoring_grandchild_after_leader_exits() {
    let mut env = Env::new(CONFIG);
    fs::write(env.path("service.sh"), FORKER).unwrap();
    env.up();
    let worker = env.pidfile("worker.pid");
    env.run(&["stop", "app"]);
    assert!(!alive(worker));
    assert!(alive(env.daemon.unwrap()));
    assert_eq!(env.state(), "stopped");
}

#[test]
fn natural_leader_exit_cleans_descendants_before_restart() {
    let mut env = Env::new(&CONFIG.replace(
        "    shutdown:",
        "    restart_policy: always\n    backoff_seconds: 30\n    shutdown:",
    ));
    fs::write(env.path("service.sh"), "sh -c 'trap \"\" TERM; echo $$ > worker.pid; exec sleep 120' &\nwhile [ ! -s worker.pid ]; do sleep 0.01; done\nexit 1\n").unwrap();
    env.up();
    let worker = env.pidfile("worker.pid");
    wait(|| env.state() == "restarting");
    assert!(!alive(worker));
    env.run(&["stop"]);
    assert_eq!(env.state(), "stopped");
    sleep(Duration::from_millis(200));
    assert_eq!(env.state(), "stopped");
}

#[test]
fn daemon_term_and_int_clean_up_services() {
    for sig in [Signal::SIGTERM, Signal::SIGINT] {
        let mut env = Env::new(CONFIG);
        fs::write(env.path("service.sh"), FORKER).unwrap();
        env.up();
        let worker = env.pidfile("worker.pid");
        signal(env.daemon.unwrap(), sig);
        wait(|| !alive(env.daemon.unwrap()));
        assert!(!alive(worker));
    }
}

#[test]
fn foreground_terminal_interrupt_and_term_stop_owned_environment() {
    for sig in [Signal::SIGINT, Signal::SIGTERM] {
        let mut env = Env::new(CONFIG);
        fs::write(env.path("service.sh"), FORKER).unwrap();
        let mut client = env.foreground();
        let worker = env.pidfile("worker.pid");
        // Terminal-style group delivery must not kill the daemon directly.
        kill(Pid::from_raw(-(client.id() as i32)), sig).unwrap();
        wait(|| client.try_wait().unwrap().is_some());
        assert!(client.wait().unwrap().success());
        assert!(!alive(worker));
        wait(|| !alive(env.daemon.unwrap()));
    }
}

#[test]
fn second_interrupt_forces_hook_and_service_cleanup() {
    let mut env = Env::new(&CONFIG.replace(
        "timeout_seconds: 1",
        "timeout_seconds: 60\n      command: exec sh hook.sh",
    ));
    fs::write(env.path("service.sh"), FORKER).unwrap();
    fs::write(
        env.path("hook.sh"),
        "trap '' TERM\necho $$ > hook.pid\nexec sleep 120\n",
    )
    .unwrap();
    let mut client = env.foreground();
    let worker = env.pidfile("worker.pid");
    signal(client.id(), Signal::SIGINT);
    let hook = env.pidfile("hook.pid");
    signal(client.id(), Signal::SIGINT);
    wait(|| client.try_wait().unwrap().is_some());
    assert!(client.wait().unwrap().success());
    assert!(!alive(worker));
    assert!(!alive(hook));
}

#[test]
fn timed_out_hook_is_killed_and_reaped() {
    let mut env = Env::new(&CONFIG.replace(
        "timeout_seconds: 1",
        "timeout_seconds: 1\n      command: exec sh hook.sh",
    ));
    fs::write(env.path("service.sh"), FORKER).unwrap();
    fs::write(env.path("hook.sh"), "echo $$ > hook.pid\nexec sleep 120\n").unwrap();
    env.up();
    let worker = env.pidfile("worker.pid");
    env.run(&["down"]);
    let hook = env.pidfile("hook.pid");
    assert!(!alive(hook));
    assert!(!alive(worker));
}

#[test]
fn dependencies_stop_after_dependents_complete() {
    let mut env = Env::new(
        "processes:\n  db:\n    command: sleep 120\n    shutdown:\n      command: echo db >> order\n  api:\n    command: sleep 120\n    depends_on:\n      db:\n        condition: process_started\n    shutdown:\n      command: sleep 0.2; echo api >> order\n",
    );
    env.up();
    wait(|| {
        let out = env.run(&["ps", "--json"]);
        serde_json::from_slice::<Value>(&out.stdout).unwrap()["processes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|p| p["state"] == "running")
    });
    env.run(&["down"]);
    assert_eq!(fs::read_to_string(env.path("order")).unwrap(), "api\ndb\n");
}

#[test]
fn stop_supersedes_in_progress_restart() {
    let mut env = Env::new(&CONFIG.replace(
        "timeout_seconds: 1",
        "timeout_seconds: 5\n      command: touch stopping; sleep 1",
    ));
    fs::write(
        env.path("service.sh"),
        "echo launch >> launches\nexec sleep 120\n",
    )
    .unwrap();
    env.up();
    wait(|| env.path("launches").exists());
    env.run(&["restart"]);
    wait(|| env.path("stopping").exists());
    env.run(&["stop"]);
    sleep(Duration::from_millis(300));
    assert_eq!(env.state(), "stopped");
    assert_eq!(
        fs::read_to_string(env.path("launches")).unwrap(),
        "launch\n"
    );
}

#[test]
fn stopping_reaps_in_flight_exec_probe() {
    let mut env = Env::new(&format!(
        "{CONFIG}    readiness_probe:\n      exec:\n        command: exec sh probe.sh\n      timeout_seconds: 60\n      period_seconds: 60\n"
    ));
    fs::write(env.path("service.sh"), "exec sleep 120\n").unwrap();
    fs::write(
        env.path("probe.sh"),
        "echo $$ > probe.pid\nexec sleep 120\n",
    )
    .unwrap();
    env.up();
    let probe = env.pidfile("probe.pid");
    env.run(&["down"]);
    assert!(!alive(probe));
}

#[test]
fn restart_waits_past_old_ten_second_limit() {
    let mut env = Env::new(&CONFIG.replace(
        "timeout_seconds: 1",
        "timeout_seconds: 20\n      command: touch stopping; sleep 11",
    ));
    fs::write(
        env.path("service.sh"),
        "echo launch >> launches\necho $$ > leader.pid\nexec sleep 120\n",
    )
    .unwrap();
    env.up();
    let leader = env.pidfile("leader.pid");
    env.run(&["restart"]);
    wait(|| env.path("stopping").exists());
    sleep(Duration::from_millis(10_300));
    assert_eq!(
        fs::read_to_string(env.path("launches")).unwrap(),
        "launch\n"
    );
    wait(|| {
        fs::read_to_string(env.path("launches"))
            .unwrap()
            .lines()
            .count()
            == 2
    });
    assert!(!alive(leader));
    env.run(&["down", "--timeout", "0"]);
}

#[test]
fn down_waits_past_old_thirty_second_limit() {
    let mut env = Env::new(&CONFIG.replace(
        "timeout_seconds: 1",
        "timeout_seconds: 40\n      command: sleep 31; touch hook-finished",
    ));
    fs::write(
        env.path("service.sh"),
        "echo $$ > leader.pid\nexec sleep 120\n",
    )
    .unwrap();
    env.up();
    let leader = env.pidfile("leader.pid");
    env.run(&["down"]);
    assert!(env.path("hook-finished").exists());
    assert!(!alive(leader));
}

#[test]
fn daemon_crash_during_down_is_not_success() {
    let mut env = Env::new(&CONFIG.replace(
        "timeout_seconds: 1",
        "timeout_seconds: 60\n      command: exec sh hook.sh",
    ));
    fs::write(env.path("service.sh"), FORKER).unwrap();
    fs::write(env.path("hook.sh"), "echo $$ > hook.pid\nexec sleep 120\n").unwrap();
    env.up();
    env.pidfile("worker.pid");
    let down = env
        .command(&["down"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    env.pidfile("hook.pid");
    signal(env.daemon.unwrap(), Signal::SIGKILL);
    let out = down.wait_with_output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("without confirming process cleanup"));
}

#[test]
fn interrupting_up_on_existing_environment_only_detaches() {
    let mut env = Env::new(CONFIG);
    fs::write(
        env.path("service.sh"),
        "echo $$ > leader.pid\nexec sleep 120\n",
    )
    .unwrap();
    env.up();
    let leader = env.pidfile("leader.pid");
    let mut client = env
        .command(&["up"])
        .process_group(0)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // Let the client install its signal handler and attach to the daemon.
    sleep(Duration::from_millis(300));
    signal(client.id(), Signal::SIGINT);
    wait(|| client.try_wait().unwrap().is_some());
    assert!(client.wait().unwrap().success());
    assert!(alive(leader));
    assert!(alive(env.daemon.unwrap()));
}

#[test]
fn tui_stays_responsive_and_accepts_force_during_shutdown() {
    use std::io::Write;
    use std::os::fd::AsRawFd;
    let mut env = Env::new(&CONFIG.replace(
        "timeout_seconds: 1",
        "timeout_seconds: 60\n      command: exec sh hook.sh",
    ));
    fs::write(env.path("service.sh"), FORKER).unwrap();
    fs::write(env.path("hook.sh"), "echo $$ > hook.pid\nexec sleep 120\n").unwrap();
    env.up();
    let worker = env.pidfile("worker.pid");
    let (mut master, slave) = terminal_pair();
    let mut command = env.command(&["tui"]);
    command
        .env("TERM", "xterm-256color")
        .stdin(slave.try_clone().unwrap())
        .stdout(slave.try_clone().unwrap())
        .stderr(slave.try_clone().unwrap());
    // SAFETY: only async-signal-safe libc calls run between fork and exec.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1
                || libc::ioctl(libc::STDIN_FILENO, libc::TIOCSCTTY as _, 0) == -1
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut tui = command.spawn().unwrap();
    // Drain screen updates so terminal output never blocks the event loop.
    let mut reader = master.try_clone().unwrap();
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut reader, &mut std::io::sink());
    });
    wait(|| {
        let mut settings = std::mem::MaybeUninit::<libc::termios>::uninit();
        // SAFETY: tcgetattr writes a complete termios on success.
        unsafe {
            libc::tcgetattr(slave.as_raw_fd(), settings.as_mut_ptr()) == 0
                && settings.assume_init().c_lflag & libc::ICANON == 0
        }
    });
    master.write_all(b"Q").unwrap();
    let hook = env.pidfile("hook.pid");
    assert!(tui.try_wait().unwrap().is_none());
    master.write_all(b"\x03").unwrap();
    wait(|| tui.try_wait().unwrap().is_some());
    assert!(tui.wait().unwrap().success());
    assert!(!alive(worker));
    assert!(!alive(hook));
}

#[test]
fn shutdown_needs_no_process_utilities_on_path() {
    let mut env = Env::new(&CONFIG.replace("exec sh", "exec /bin/sh"));
    fs::write(
        env.path("service.sh"),
        FORKER
            .replace("sh -c", "/bin/sh -c")
            .replace("exec sleep", "exec /bin/sleep"),
    )
    .unwrap();
    fs::create_dir(env.path("empty-bin")).unwrap();
    let out = env
        .command(&["up", "-d", "--json"])
        .env("PATH", env.path("empty-bin"))
        .env("COMPOSE_SHELL", "/bin/sh")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    env.daemon = Some(
        serde_json::from_slice::<Value>(&out.stdout).unwrap()["pid"]
            .as_u64()
            .unwrap() as u32,
    );
    let worker = env.pidfile("worker.pid");
    // The daemon inherited an empty PATH, so any internal ps invocation fails.
    env.run(&["down"]);
    assert!(!alive(worker));
}

fn terminal_pair() -> (fs::File, fs::File) {
    use std::os::fd::FromRawFd;
    let (mut master_fd, mut slave_fd) = (-1, -1);
    let mut size = libc::winsize {
        ws_row: 24,
        ws_col: 100,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: openpty receives valid writable fd pointers and a valid size;
    // optional terminal settings/name pointers are null.
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master_fd,
                &mut slave_fd,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::addr_of_mut!(size),
            )
        },
        0
    );
    // SAFETY: successful openpty returned two new, uniquely owned descriptors.
    let master = unsafe { fs::File::from_raw_fd(master_fd) };
    // SAFETY: as above; this descriptor is distinct from master_fd.
    let slave = unsafe { fs::File::from_raw_fd(slave_fd) };
    (master, slave)
}

#[test]
fn default_pager_launches_directly_without_shell_on_path() {
    use std::os::unix::fs::PermissionsExt;
    let mut env = Env::new("processes:\n  app:\n    command: echo pager-output; exec sleep 120\n");
    env.up();
    wait(|| {
        fs::read_dir(env.path("state/decompose"))
            .unwrap()
            .flatten()
            .any(|entry| {
                entry.path().extension().is_some_and(|ext| ext == "log")
                    && fs::read_to_string(entry.path())
                        .unwrap()
                        .contains("pager-output")
            })
    });
    fs::create_dir(env.path("pager-bin")).unwrap();
    let pager = env.path("pager-bin/less");
    fs::write(
        &pager,
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > pager-args\n/bin/cat > pager-input\n",
    )
    .unwrap();
    fs::set_permissions(&pager, fs::Permissions::from_mode(0o755)).unwrap();
    let (_master, slave) = terminal_pair();
    let out = env
        .command(&["logs"])
        .env_remove("PAGER")
        .env_remove("DECOMPOSE_PAGER")
        .env("PATH", env.path("pager-bin"))
        .stdin(Stdio::null())
        .stdout(slave)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(fs::read_to_string(env.path("pager-args")).unwrap(), "-R\n");
    assert!(
        fs::read_to_string(env.path("pager-input"))
            .unwrap()
            .contains("pager-output")
    );
}
