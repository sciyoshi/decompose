//! Exercise the real terminal gate and delivery of reports without newlines.
use super::*;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use std::io::Read;
use std::os::fd::FromRawFd;

fn terminal_command(mut command: Command, interrupt_wait: bool) -> Output {
    let mut master = -1;
    let mut slave = -1;
    let mut size = libc::winsize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: openpty receives valid output pointers and a valid window size;
    // null name/termios select defaults. Successful descriptors are owned below.
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                // libc declares winp as *const on Linux and *mut on macOS.
                &raw mut size,
            )
        },
        0
    );
    // Keep the extra descriptors out of the daemon and services. Command
    // duplicates the slave onto stdout, which intentionally remains open.
    for fd in [master, slave] {
        // SAFETY: fd is a valid newly opened descriptor; F_SETFD takes flags.
        assert_eq!(
            unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) },
            0
        );
    }
    // SAFETY: both descriptors are newly opened and uniquely owned.
    let (mut master, slave) =
        unsafe { (fs::File::from_raw_fd(master), fs::File::from_raw_fd(slave)) };
    command
        .stdin(Stdio::null())
        .stdout(slave)
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("spawn terminal client");
    // Command retains its configured slave descriptor after spawn.
    drop(command);
    let pid = child.id();
    let reader = thread::spawn(move || {
        let mut output = Vec::new();
        let mut buf = [0; 4096];
        let mut interrupted = false;
        // Wait for a running child, not merely a pending startup snapshot.
        let waiting = STANDARD
            .encode("Starting environment: 0/1 services ready; sleeper: waiting for readiness");
        loop {
            match master.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    output.extend_from_slice(&buf[..n]);
                    if interrupt_wait
                        && !interrupted
                        && output
                            .windows(waiting.len())
                            .any(|s| s == waiting.as_bytes())
                    {
                        kill(Pid::from_raw(pid as i32), Signal::SIGINT).unwrap();
                        interrupted = true;
                    }
                }
                // Linux PTYs return EIO after the last slave closes.
                Err(e) if e.raw_os_error() == Some(libc::EIO) => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => panic!("read terminal: {e}"),
            }
        }
        output
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let mut timed_out = false;
    while child.try_wait().unwrap().is_none() {
        if std::time::Instant::now() >= deadline {
            child.kill().unwrap();
            timed_out = true;
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let mut result = child.wait_with_output().unwrap();
    result.stdout = reader.join().unwrap();
    assert!(!timed_out, "terminal command timed out: {result:?}");
    result
}

fn reports(output: &[u8]) -> Vec<(String, String)> {
    String::from_utf8_lossy(output)
        .split("\x1b]7501;")
        .skip(1)
        .map(|rest| {
            let report = rest.split_once("\x1b\\").expect("complete OSC").0;
            let field = |key: &str| {
                report
                    .split(':')
                    .find_map(|pair| pair.strip_prefix(key))
                    .unwrap()
            };
            assert_eq!(field("app="), "decompose");
            let message = String::from_utf8(STANDARD.decode(field("msg=")).unwrap()).unwrap();
            (field("state=").to_owned(), message)
        })
        .collect()
}

#[test]
fn startup_reports_terminal_outcomes_and_respects_output_gates() {
    let (root, project, runtime, state, config) = setup_project();
    let home = root.path().join("home");
    let command = |args: &[&str]| {
        let mut command = Command::new(bin_path());
        command
            .current_dir(&project)
            .args(args)
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("XDG_STATE_HOME", &state)
            .env("HOME", &home)
            .env("TERM", "xterm-256color")
            .env("NO_COLOR", "1")
            .env("DECOMPOSE_DAEMON_READY_POLL_MS", "20")
            .env_remove("DECOMPOSE_NO_PROGRAM_STATUS")
            .env_remove("DECOMPOSE_SESSION");
        command
    };
    let down = || command(&["down"]).output().unwrap();
    let checks = std::panic::catch_unwind(|| {
        for (args, message) in [
            (vec!["up"], "Environment ready"),
            (vec!["up", "-d"], "Startup requested"),
            (vec!["up", "--no-start"], "Environment registered"),
        ] {
            let output = terminal_command(command(&args), false);
            assert_success(&output, "terminal up");
            let status = reports(&output.stdout);
            assert_eq!(status.first().unwrap().0, "working");
            assert_eq!(status.last().unwrap(), &("done".into(), message.into()));
            assert_success(&down(), "down");
        }

        for (args, variable) in [
            (vec!["up", "--json"], None),
            (vec!["up"], Some(("TERM", "dumb"))),
            (vec!["up"], Some(("DECOMPOSE_NO_PROGRAM_STATUS", "1"))),
        ] {
            let mut cmd = command(&args);
            if let Some((key, value)) = variable {
                cmd.env(key, value);
            }
            let output = terminal_command(cmd, false);
            assert_success(&output, "gated terminal up");
            assert!(reports(&output.stdout).is_empty());
            if args.contains(&"--json") {
                let value: Value = serde_json::from_slice(&output.stdout).unwrap();
                assert_eq!(value["readiness"], "satisfied");
            }
            assert_success(&down(), "down");
        }
        let output = command(&["up"]).output().unwrap();
        assert_success(&output, "redirected up");
        assert!(reports(&output.stdout).is_empty());
        assert_success(&down(), "down");

        // A probe that cannot pass exercises both timeout and interruption.
        fs::write(&config, "processes:\n  sleeper:\n    command: sleep 30\n    readiness_probe:\n      exec:\n        command: 'false'\n").unwrap();
        for interrupted in [false, true] {
            let mut cmd = command(&["up"]);
            cmd.env(
                "DECOMPOSE_DAEMON_READY_TIMEOUT_MS",
                if interrupted { "10000" } else { "1000" },
            );
            let output = terminal_command(cmd, interrupted);
            assert_eq!(
                output.status.code(),
                Some(if interrupted { 130 } else { 1 })
            );
            let status = reports(&output.stdout);
            let last = status.last().unwrap();
            assert_eq!(last.0, if interrupted { "idle" } else { "error" });
            assert!(last.1.contains(if interrupted {
                "environment remains running"
            } else {
                "timed out"
            }));
            let ps = command(&["ps", "--json"]).output().unwrap();
            let ps: Value = serde_json::from_slice(&ps.stdout).unwrap();
            assert_eq!(ps["processes"][0]["state"], "running");
            assert_success(&down(), "down");
        }

        // Config resolution fails before the readiness loop or daemon startup.
        fs::write(&config, "processes: [invalid]").unwrap();
        let output = terminal_command(command(&["up"]), false);
        assert!(!output.status.success());
        let status = reports(&output.stdout);
        assert_eq!(status.first().unwrap().0, "working");
        assert_eq!(status.last().unwrap().0, "error");
    });
    let _ = down();
    if let Err(error) = checks {
        std::panic::resume_unwind(error);
    }
}
