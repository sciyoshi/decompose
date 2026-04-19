//! Runtime tuning knobs.
//!
//! A handful of timeouts and intervals that most users should never touch,
//! but power users occasionally need to override — for example, bumping the
//! daemon-ready wait when orchestrating slow-starting JVM services.
//!
//! These are not part of the YAML schema; they're controlled by process
//! environment variables so they can be set once per shell without churning
//! per-project config files.
//!
//! Precedence and fallback: each getter reads a single env var, parses it as
//! `u64` milliseconds, and falls back to the documented default. Malformed
//! values emit a one-line warning to stderr and fall back to the default
//! (rather than failing the command).
//!
//! | Env var                              | Default | Description                                                        |
//! |--------------------------------------|--------:|--------------------------------------------------------------------|
//! | `DECOMPOSE_DAEMON_READY_TIMEOUT_MS`  | 300000  | How long the CLI waits for the daemon to report all services ready |
//! | `DECOMPOSE_DAEMON_READY_POLL_MS`     |     500 | Poll interval while waiting for daemon startup                     |
//! | `DECOMPOSE_IPC_TIMEOUT_MS`           |    5000 | Per-request IPC round-trip timeout                                 |
//! | `DECOMPOSE_SUPERVISOR_TICK_MS`       |     150 | Supervisor loop tick interval inside the daemon                    |
//! | `DECOMPOSE_ORPHAN_TIMEOUT`           |      30 | Grace period (seconds) after parent death before auto-exit         |
//! | `DECOMPOSE_ORPHAN_CHECK_MS`          |    1000 | Orphan watchdog tick interval inside the daemon                    |

use std::time::Duration;

pub const ENV_DAEMON_READY_TIMEOUT_MS: &str = "DECOMPOSE_DAEMON_READY_TIMEOUT_MS";
pub const ENV_DAEMON_READY_POLL_MS: &str = "DECOMPOSE_DAEMON_READY_POLL_MS";
pub const ENV_IPC_TIMEOUT_MS: &str = "DECOMPOSE_IPC_TIMEOUT_MS";
pub const ENV_SUPERVISOR_TICK_MS: &str = "DECOMPOSE_SUPERVISOR_TICK_MS";
pub const ENV_ORPHAN_TIMEOUT_SECS: &str = "DECOMPOSE_ORPHAN_TIMEOUT";
pub const ENV_ORPHAN_CHECK_MS: &str = "DECOMPOSE_ORPHAN_CHECK_MS";

pub const DEFAULT_DAEMON_READY_TIMEOUT_MS: u64 = 300_000;
pub const DEFAULT_DAEMON_READY_POLL_MS: u64 = 500;
pub const DEFAULT_IPC_TIMEOUT_MS: u64 = 5_000;
pub const DEFAULT_SUPERVISOR_TICK_MS: u64 = 150;
pub const DEFAULT_ORPHAN_TIMEOUT_SECS: u64 = 30;
pub const DEFAULT_ORPHAN_CHECK_MS: u64 = 1_000;

/// Read a `u64` millisecond value from the given env var, falling back to
/// `default_ms` if the var is unset. Malformed or zero values are logged to
/// stderr and also fall back to the default.
pub fn duration_ms_from_env(name: &str, default_ms: u64) -> Duration {
    Duration::from_millis(millis_from_env(name, default_ms))
}

/// Same as [`duration_ms_from_env`] but returns the raw millisecond value
/// so callers can do further arithmetic (e.g. build deadlines).
pub fn millis_from_env(name: &str, default_ms: u64) -> u64 {
    match std::env::var(name) {
        Ok(raw) => match raw.trim().parse::<u64>() {
            Ok(n) if n > 0 => n,
            Ok(_) => {
                eprintln!(
                    "warning: {name}={raw:?} is zero or negative; using default {default_ms}ms"
                );
                default_ms
            }
            Err(_) => {
                eprintln!(
                    "warning: {name}={raw:?} is not a valid u64 millisecond value; using default {default_ms}ms"
                );
                default_ms
            }
        },
        Err(_) => default_ms,
    }
}

pub fn daemon_ready_timeout() -> Duration {
    duration_ms_from_env(ENV_DAEMON_READY_TIMEOUT_MS, DEFAULT_DAEMON_READY_TIMEOUT_MS)
}

pub fn daemon_ready_poll() -> Duration {
    duration_ms_from_env(ENV_DAEMON_READY_POLL_MS, DEFAULT_DAEMON_READY_POLL_MS)
}

pub fn ipc_timeout() -> Duration {
    duration_ms_from_env(ENV_IPC_TIMEOUT_MS, DEFAULT_IPC_TIMEOUT_MS)
}

pub fn supervisor_tick() -> Duration {
    duration_ms_from_env(ENV_SUPERVISOR_TICK_MS, DEFAULT_SUPERVISOR_TICK_MS)
}

/// Grace period between parent-death detection and daemon auto-exit, during
/// which any IPC client activity resets the clock. Specified in seconds to
/// match user expectations (`DECOMPOSE_ORPHAN_TIMEOUT=5`).
pub fn orphan_timeout() -> Duration {
    let secs = millis_from_env(ENV_ORPHAN_TIMEOUT_SECS, DEFAULT_ORPHAN_TIMEOUT_SECS);
    Duration::from_secs(secs)
}

/// How often the orphan watchdog polls the parent PID.
pub fn orphan_check_interval() -> Duration {
    duration_ms_from_env(ENV_ORPHAN_CHECK_MS, DEFAULT_ORPHAN_CHECK_MS)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Guard that sets an env var for the duration of its lifetime, then
    /// restores the previous value (or removes it) on drop. Lets each test
    /// mutate process-wide env without leaking into sibling tests.
    ///
    /// # Safety
    /// `std::env::set_var` / `remove_var` are `unsafe` starting in Rust 2024
    /// because they mutate process-global state. Tests within this module
    /// use distinct var names so they don't collide with each other.
    struct EnvGuard {
        name: &'static str,
        previous: Option<String>,
    }

    impl EnvGuard {
        fn set(name: &'static str, value: &str) -> Self {
            let previous = std::env::var(name).ok();
            unsafe {
                std::env::set_var(name, value);
            }
            Self { name, previous }
        }

        fn unset(name: &'static str) -> Self {
            let previous = std::env::var(name).ok();
            unsafe {
                std::env::remove_var(name);
            }
            Self { name, previous }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.previous {
                    Some(v) => std::env::set_var(self.name, v),
                    None => std::env::remove_var(self.name),
                }
            }
        }
    }

    const TEST_VAR_VALID: &str = "DECOMPOSE_TUNING_TEST_VALID";
    const TEST_VAR_MISSING: &str = "DECOMPOSE_TUNING_TEST_MISSING";
    const TEST_VAR_MALFORMED: &str = "DECOMPOSE_TUNING_TEST_MALFORMED";
    const TEST_VAR_ZERO: &str = "DECOMPOSE_TUNING_TEST_ZERO";

    #[test]
    fn parses_valid_value() {
        let _g = EnvGuard::set(TEST_VAR_VALID, "12345");
        assert_eq!(millis_from_env(TEST_VAR_VALID, 999), 12_345);
    }

    #[test]
    fn uses_default_when_missing() {
        let _g = EnvGuard::unset(TEST_VAR_MISSING);
        assert_eq!(millis_from_env(TEST_VAR_MISSING, 777), 777);
    }

    #[test]
    fn uses_default_when_malformed() {
        let _g = EnvGuard::set(TEST_VAR_MALFORMED, "not-a-number");
        assert_eq!(millis_from_env(TEST_VAR_MALFORMED, 321), 321);
    }

    #[test]
    fn uses_default_when_zero() {
        let _g = EnvGuard::set(TEST_VAR_ZERO, "0");
        assert_eq!(millis_from_env(TEST_VAR_ZERO, 500), 500);
    }
}
