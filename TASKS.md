# decompose — improvement tasks

## 1. Docker Compose CLI compatibility

The CLI should broadly match `docker compose` so users can switch without
relearning. These are the structural changes needed.

### 1a. Promote start/stop/restart to top-level commands

Currently `decompose process start <name>`. Docker Compose uses
`docker compose start [SERVICE...]`.

- Move `start`, `stop`, `restart` to top-level `Commands` enum.
- Accept multiple service names (not just one).
- Keep `process scale` as a subcommand (docker compose uses `up --scale`
  but `scale` as a subcommand is fine).
- Consider keeping `process` subcommand as an alias for backward compat.

### 1b. Add `-f` short flag for `logs --follow`

Docker Compose: `docker compose logs -f [SERVICE...]`
Current: `decompose logs --follow [SERVICE...]`

Add `#[arg(short = 'f', long = "follow")]` to `LogsArgs.follow`.

Note: `-f` is also used for `--file` on global args. Since `logs` has its
own arg struct via `#[command(flatten)]`, clap should handle this — but
verify there's no conflict.

### 1c. Add `config` command

`docker compose config` validates and prints the resolved config.

- Parse and merge all config files.
- Apply interpolation.
- Print the resolved YAML to stdout.
- With `--json`, print as JSON instead.
- Useful for debugging merge/interpolation issues.

### 1d. Add `kill` command

`docker compose kill [-s SIGNAL] [SERVICE...]`

- Send a signal to running processes without the graceful shutdown sequence.
- Default signal: SIGKILL (9), overridable with `-s`.
- Difference from `stop`: `stop` runs the shutdown sequence (command ->
  signal -> timeout -> SIGKILL). `kill` sends the signal immediately.

### 1e. Add `ls` command

`docker compose ls` lists running compose projects.

- Scan the socket directory for active `.sock` files.
- Ping each to check if the daemon is alive.
- Print project name, status, and config path.

### 1f. Flatten UpArgs to embed GlobalArgs

`UpArgs` duplicates `config_files`, `session`, `env_files`,
`disable_dotenv`, and `output` from `GlobalArgs`. Should use
`#[command(flatten)] pub global: GlobalArgs` plus the up-specific fields.
Update `run_up` to use `args.global.*` accordingly.

## 2. Build and dependency hygiene

### 2a. Pin dependency versions in Cargo.toml

All dependencies currently use `version = "*"`. Pin to actual semver
ranges (e.g., `anyhow = "1"`, `tokio = { version = "1", ... }`). Use the
versions currently in Cargo.lock.

### 2b. Migrate off deprecated serde_yaml

`serde_yaml` is deprecated (visible in cargo output). Evaluate and
migrate to a maintained alternative. Candidates:

- `serde_yml` — drop-in fork of serde_yaml
- `yaml-rust2` + manual serde integration

`serde_yml` is likely the lowest-friction path.

## 3. Correctness and robustness

### 3a. Dependency cycle detection

If process A depends on B and B depends on A, both stay Pending forever.
`validate_config` should detect cycles (topological sort or DFS) and
return a clear error.

### 3b. Stale daemon cleanup

If the daemon crashes, socket and pid files persist. On `up`, check the
pid file — if the process isn't alive, clean up stale files and proceed
with a fresh daemon.

### 3c. IPC timeout

`send_request` in `ipc.rs` has no timeout. A hung daemon blocks the CLI
forever. Wrap the socket connect + read in `tokio::time::timeout`.

### 3d. Use libc for signal sending

`daemon.rs` shells out to `kill -N <pid>` to send signals. Use
`libc::kill()` or the `nix` crate directly — more reliable and doesn't
depend on `kill` being on PATH.

### 3e. Use `-c` instead of `-lc` for shell commands

`build_shell_command` uses `bash -lc` which starts a login shell. This
sources `.bash_profile` etc., which is slow and can have side effects.
Use `-c` instead, matching what docker compose and most process managers
do.

## 4. Code quality

### 4a. Extract stdout/stderr capture helper in daemon.rs

The pattern for piping process output and checking `ready_log_line` is
duplicated 4 times (stdout/stderr x initial spawn/restart). Extract into
a single helper function.

### 4b. Extract log-tailing helper in lib.rs

`stream_daemon_logs` and `stream_filtered_logs` share the same
file-tailing logic. Factor out the common polling/reading code.

### 4c. Remove dead code

`resolve_config_path` (singular) in `config.rs` appears unused. Remove
it, or if it's needed for a public API, mark it so.

### 4d. Dynamic table column widths

Table formatting in `lib.rs` uses fixed `{:<24}` widths. Compute column
widths from actual data to handle long process names.

### 4e. Fix scale-down replica cleanup

`handle_client` in `daemon.rs` has a scale-down path that signals
replicas to stop but never removes them from the process map after they
terminate.

## 5. Native HTTP health checks

The `http_get` health check currently shells out to `curl`. This fails
silently if curl isn't installed. Options:

- Add `reqwest` with minimal features (`rustls-tls`, no default features).
- Or do a raw TCP connect + minimal HTTP/1.1 request with tokio.

The `reqwest` approach is simpler and more correct.

## 6. Test coverage

### 6a. Dependency cycle detection tests

Once cycle detection is implemented (3a), add unit tests for simple
cycles, transitive cycles, and self-dependencies.

### 6b. Health check and restart integration tests

Behavioral tests for:
- Process that restarts on failure with backoff.
- Health check transitions from unhealthy -> healthy.
- `max_restarts` cap is respected.

### 6c. Shutdown sequence tests

Verify the signal -> timeout -> SIGKILL sequence works correctly.

### 6d. JSON output snapshot tests

Capture `ps --json` output for known configs and assert the structure
is stable across changes.

### 6e. Session isolation integration tests

Two instances with different `--session` values should not interfere.

### 6f. Fix output.rs test

`env_truthy_parses_common_values` reimplements logic inline instead of
calling the actual `env_truthy` function. Fix it to test the real code
path using env var manipulation.
