# Decompose

A simple process orchestrator for local development. Broadly compatible with
Docker Compose CLI and YAML semantics, but runs native processes instead of
containers.

## Design

- **Daemon architecture**: `decompose up` spawns a background daemon per
  project. The daemon manages process lifecycles and communicates via local
  socket IPC (using the `interprocess` crate). The CLI is a thin client that
  sends JSON requests over the socket.
- **Instance identity**: Each environment is identified by a SHA-256 hash of
  the config directory + file set, or by an explicit `--session` name. This
  allows multiple isolated environments and cross-tab targeting.
- **XDG paths**: Sockets go to `$XDG_RUNTIME_DIR/decompose/`, state (pid, log)
  goes to `$XDG_STATE_HOME/decompose/`, with sensible fallbacks.

## CLI

Goal: broad compatibility with `docker compose` commands. The main commands
should feel familiar to Docker Compose users.

```
decompose up [-f FILE...] [-d] [--no-deps] [SERVICE...]
decompose down [-f FILE...]
decompose ps [-f FILE...]
decompose logs [-f] [-n N] [SERVICE...]
decompose start [SERVICE...]
decompose stop [SERVICE...]
decompose restart [SERVICE...]
decompose kill [SERVICE...]
decompose config
decompose ls
```

Output modes: `--json`, `--table`, or auto-detect (TTY/CI/LLM -> table,
otherwise JSON).

## Configuration

Config files are discovered in order: `compose.yml`, `compose.yaml`,
`decompose.yml`, `decompose.yaml`. Multiple `-f` flags merge with overlay
semantics (later files override earlier ones).

### YAML schema

```yaml
# Global settings
environment:           # Global env vars (map or list of KEY=VALUE)
exit_mode: wait_all    # wait_all | exit_on_failure | exit_on_end
disable_env_expansion: false

processes:
  service_name:
    command: "..."                    # Required. Shell command to run.
    description: "..."               # Optional description.
    working_dir: "/path"             # Defaults to config directory.
    environment:                     # Per-process env vars (map or list).
      KEY: value
    env_file: ["extra.env"]          # Additional .env files to load.
    disabled: false                  # Visible but not auto-started.
    replicas: 1                      # Number of instances.
    ready_log_line: "regex pattern"  # Sets log_ready when matched in output.
    restart_policy: "no"             # no | on_failure | always
    backoff_seconds: 1               # Delay between restart attempts.
    max_restarts: null               # Cap on restart count (null = unlimited).

    depends_on:
      other_service:
        condition: process_started
        # Conditions: process_started, process_completed,
        #   process_completed_successfully, process_healthy,
        #   process_log_ready

    readiness_probe:                 # Sets healthy flag when passing.
      exec:
        command: "curl -f localhost:8080/health"
      period_seconds: 10
      timeout_seconds: 1
      initial_delay_seconds: 0
      success_threshold: 1
      failure_threshold: 3

    liveness_probe:                  # Same schema as readiness_probe.
      http_get:
        host: "127.0.0.1"
        port: 8080
        scheme: http
        path: /

    shutdown:
      command: "cleanup.sh"          # Optional pre-shutdown command.
      signal: 15                     # Signal to send (default SIGTERM).
      timeout_seconds: 10            # Wait before SIGKILL.
```

### Environment variable precedence (lowest to highest)

1. `.env` file (auto-loaded unless `--disable-dotenv`)
2. Explicit `-e` env files
3. Global `environment` block
4. Per-process `env_file` entries
5. Per-process `environment` block

### Variable interpolation

- `${VAR}` and `$VAR` — substitute from env
- `${VAR:-default}` — substitute with fallback
- `$$` — literal `$` escape
- Disabled globally with `disable_env_expansion: true`

Applied to: `command`, `description`, `working_dir`, `ready_log_line`,
`shutdown.command`, and environment values.

## Architecture

```
src/
  main.rs      Entry point, calls run_cli()
  lib.rs       CLI command handlers, log streaming
  cli.rs       Clap argument definitions
  config.rs    YAML parsing, merging, .env loading, interpolation
  daemon.rs    Daemon process lifecycle, supervisor loop, IPC handlers
  model.rs     Core types (ProcessStatus, HealthProbe, etc.)
  ipc.rs       Request/Response protocol, socket helpers
  output.rs    JSON/table output formatting
  paths.rs     XDG path management, instance ID generation
```
