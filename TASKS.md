# decompose — improvement tasks

Priority-ordered improvements. Items marked `[x]` are complete.

## 1. Docker Compose CLI compatibility

The CLI should broadly match `docker compose` so users can switch without
relearning.

- [x] **1a. Promote start/stop/restart to top-level commands.** Accept multiple
  service names. Empty list = operate on all services. Unknown names return a
  clear error. Integration-tested.
- [x] **1f. Flatten `UpArgs` to embed `GlobalArgs`.** Removes duplication of
  `config_files`, `session`, `env_files`, `disable_dotenv`, `output`.
- [ ] **1b. Promote `-f/--file`, `-e/--env-file`, `--session`, `--disable-dotenv`
  to true global flags on `Cli`** instead of per-subcommand. Unblocks
  `logs -f` (currently `-f` is taken by `--file` within LogsArgs). Matches
  `docker compose -f FILE <subcmd>` shape.
- [ ] **1c. Add `config` command.** Validate and print the resolved config (YAML
  by default, JSON with `--json`). Useful for debugging merges and interpolation.
- [ ] **1d. Add `kill` command.** `kill [-s SIGNAL] [SERVICE...]` — send a
  signal immediately without running the shutdown sequence. Default SIGKILL.
- [ ] **1e. Add `ls` command.** List running decompose instances by scanning
  the socket directory and pinging each daemon.
- [ ] **1g. Add `--timeout` to `down`** to override per-process
  `shutdown.timeout_seconds` at the CLI level.
- [ ] **1h. Add `--wait` to `up -d`** that blocks until all services are
  healthy (when readiness probes are configured) or started.
- [ ] **1i. Add `--remove-orphans` to `up`.** If the config has removed a
  process, stop its remaining replicas in the running daemon.
- [ ] **1j. Add `exec` and `run` commands.** `exec SERVICE CMD...` attaches a
  one-off command to a running service's environment; `run SERVICE CMD...`
  executes a one-off command in a service context without attaching. Useful
  for debugging (e.g., `decompose exec db psql`).

## 2. Build and dependency hygiene

- [x] **2a. Pin dependency versions in Cargo.toml.** All deps now use real
  semver constraints (`1`, `4`, `0.9`, etc.).
- [ ] **2b. Migrate off deprecated serde_yaml.** Candidates: `serde_yml` (fork)
  or `yaml-rust2`. `serde_yml` is the lowest-friction drop-in.

## 3. Correctness and robustness

- [x] **3a. Dependency cycle detection.** `validate_config` now runs a
  three-color DFS over `depends_on` and reports the cycle path
  (e.g. `a -> b -> a`). `run_up` does a pre-flight `load_and_merge_configs`
  call so the error is surfaced at the CLI immediately instead of
  manifesting as a generic "daemon did not become ready" timeout.
- [x] **3b. Stale daemon cleanup.** `run_up` now removes orphaned socket/pid
  files before spawning a fresh daemon when the existing socket fails to
  respond to a Ping. Belt-and-suspenders with the daemon's own cleanup.
- [x] **3c. IPC timeout.** `send_request` is now wrapped in a 5-second
  `tokio::time::timeout`. A hung daemon produces a clear error instead of
  blocking the CLI forever.
- [ ] **3d. Use libc (or nix crate) for signal sending.** Currently shells out
  to `kill -N <pid>`; more fragile than a direct syscall.
- [x] **3e. Use `-c` instead of `-lc` for shell commands.** `build_shell_command`
  now uses `sh -c` (matching docker compose) instead of `bash -lc`. Default
  shell changed from `bash` to `sh`; users can override via `COMPOSE_SHELL`.

## 4. UX polish (from baseline QA)

- [x] **4a. Graceful `down` when no daemon is running.** Mirrors `ps` behavior:
  exit 0 with a clear message instead of surfacing a raw connection error.
- [x] **4b. Human-readable `up` status text.** "already running" instead of
  "already_running" (enum-looking).
- [x] **4c. Actionable `attach`/`logs` error when no environment.** Suggests
  `decompose up` to start one.
- [x] **4d. Ports command returns Error instead of Ack.** Stubbed `ports *`
  subcommands previously exited 0 with a misleading "ok" status.
- [x] **4e. `logs` with no output hints why.** Prints a stderr note when the
  log file is empty or the service filter matched nothing.
- [ ] **4f. Top-level intro text.** Bare `decompose` should print a short
  orientation with a quick-start example, not just clap's auto-generated help.
- [ ] **4g. Dynamic table column widths.** Fixed `{:<24}` in `emit_ps`
  misaligns with long service names.
- [ ] **4h. `ps` table should show health/restart columns.** Currently only
  NAME and STATUS; hide healthy/restart fields are only exposed in JSON.
- [ ] **4i. Preserve daemon log across restarts** or add explicit rotation.
  Currently truncated on every `up`, losing post-mortem logs.

## 5. Code quality

- [ ] **5a. Extract stdout/stderr capture helper in daemon.rs.** The
  pipe+read+ready_log_line pattern is duplicated 4 times (stdout/stderr x
  initial spawn/restart).
- [ ] **5b. Extract log-tailing helper in lib.rs.** `stream_daemon_logs` and
  `stream_filtered_logs` share the same file-tailing logic.
- [x] **5c. Remove dead code.** `resolve_config_path` (singular) removed.
- [ ] **5d. Fix scale-down replica cleanup.** `handle_client` signals excess
  replicas to stop but never removes them from the process map.

## 6. Native HTTP health checks

- [ ] **6. Replace `curl` shell-out** in `http_get` probes with either:
  - `reqwest` (minimal features: rustls-tls, no default features), or
  - a raw TCP connect + minimal HTTP/1.1 request with tokio.
  The `reqwest` approach is simpler and more correct. Currently fails
  silently if `curl` isn't on PATH.

## 7. Test coverage

- [ ] **7a. Dependency cycle detection tests** — after 3a is done.
- [ ] **7b. Health check and restart integration tests** — restart with
  backoff, health transitions, `max_restarts` cap respected.
- [ ] **7c. Shutdown sequence tests** — signal → timeout → SIGKILL flow.
- [ ] **7d. JSON output snapshot tests** for `ps` output stability.
- [ ] **7e. Session isolation integration tests** — two `--session` values
  should not interfere.
- [x] **7f. Fix `output.rs` `env_truthy` test.** Now calls the real
  `env_truthy` function against actual env vars with unique keys per case.
