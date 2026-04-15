# decompose

![decompose logo](assets/logo.svg)

**Run your stack at native speed.**  
`decompose` is a Rust process orchestrator for local development and agentic coding loops.

No image builds. No container cold starts. No bridge-network translation overhead.  
Just your real processes, fast, with a familiar compose-like interface.

## Installing

### From crates.io

```bash
cargo install decompose
```

Requires Rust 1.85 or later. If you don't have Rust installed, grab it from [rustup.rs](https://rustup.rs/).

### Prebuilt binaries

Download a tarball for your platform from the [latest release](https://github.com/sciyoshi/decompose/releases/latest), extract it, and put `decompose` on your `$PATH`. Builds are published for:

| Target | OS | Arch |
|---|---|---|
| `x86_64-unknown-linux-gnu` | Linux | x86_64 |
| `aarch64-unknown-linux-gnu` | Linux | ARM64 |
| `x86_64-apple-darwin` | macOS | Intel |
| `aarch64-apple-darwin` | macOS | Apple Silicon |

Quick install example (macOS Apple Silicon):

```bash
curl -sL https://github.com/sciyoshi/decompose/releases/latest/download/decompose-aarch64-apple-darwin.tar.gz \
  | tar xz -C /usr/local/bin
```

### With Nix

Run without installing:

```bash
nix run github:sciyoshi/decompose -- up
```

Or install into your profile:

```bash
nix profile install github:sciyoshi/decompose
```

You can also add it as a flake input in your own `flake.nix`:

```nix
inputs.decompose.url = "github:sciyoshi/decompose";
```

The flake also exposes a `devShell` for contributors — `nix develop` drops you into a shell with `cargo`, `rustc`, `rustfmt`, and `clippy` pinned.

### From source

```bash
git clone https://github.com/sciyoshi/decompose
cd decompose
cargo build --release
```

The binary will be at `target/release/decompose`. You can also use `cargo install --path .` to install it directly into your Cargo bin directory.

## Why this is better for day-to-day coding

- **Native performance**: run directly on host processes and filesystems.
- **Faster inner loops**: no Dockerfile rebuilds just to iterate on app code.
- **Lower complexity**: no container networking setup for every local workflow.
- **Agent-friendly**: predictable JSON/table output and deterministic control from other tabs.
- **Familiar UX**: `up`, `ps`, `down`, compose-style YAML, dependencies, replicas.

## Built for humans and agents

- `decompose up` starts and attaches.
- `Ctrl-C` detaches your terminal session while keeping the daemon alive.
- `decompose up -d` starts and returns immediately.
- `decompose ps` reports empty state instead of error when nothing is running.
- Use `decompose down` from any tab/agent to stop the environment.

## Reproducible with Nix

This repo ships a `flake.nix` so you can pair **Nix + decompose** and get most of Docker's local-dev benefits (isolated environments, consistent versions across machines) without container runtime overhead.

```bash
nix develop
cargo test
```

Nix pins the toolchain and dependencies; `decompose` orchestrates native processes on top of that reproducible environment.

## Commands

```
decompose up [-d|--detach] [--no-deps] [SERVICE...]
decompose down
decompose ps
decompose attach
decompose logs [-f|--follow] [-n|--tail N] [SERVICE...]
decompose start [SERVICE...]
decompose stop [SERVICE...]
decompose restart [SERVICE...]
```

Global flags (`--file`, `--session`, `-e`, `--disable-dotenv`, `--json`,
`--table`) go **before** the subcommand:

```bash
decompose --file compose.yml --session myproject ps --json
```

## CLI usage examples

### Basic lifecycle

```bash
# Start everything in the background
decompose up -d

# Check what is running
decompose ps

# Follow all logs
decompose logs -f

# Tear down the environment
decompose down
```

### Starting specific services

```bash
# Start only the web and api services (dependencies are started automatically)
decompose up -d web api

# Start services without pulling in dependencies
decompose up -d --no-deps web
```

### Managing individual services

```bash
# Stop a single service
decompose stop worker

# Start it back up
decompose start worker

# Restart one or more services
decompose restart web api
```

### Viewing logs

```bash
# Stream logs from all services
decompose logs -f

# Show the last 100 lines from a specific service
decompose logs -n 100 web

# Follow logs for two services
decompose logs -f api worker
```

### Multi-file configuration

```bash
# Merge a base config with development overrides
decompose --file base.yml --file dev.yml up -d

# Check status using the same file set
decompose --file base.yml --file dev.yml ps
```

### Output modes

```bash
# Machine-readable JSON (useful in scripts and CI)
decompose ps --json

# Human-friendly table
decompose ps --table

# Pipe JSON into jq
decompose ps --json | jq '.[] | select(.status == "running")'
```

### Session isolation

```bash
# Run two independent environments from the same directory
decompose --session staging up -d
decompose --session canary  up -d

# Inspect each independently
decompose --session staging ps
decompose --session canary  ps

# Tear down one without affecting the other
decompose --session staging down
```

### Attaching to a running environment

```bash
# Start detached, then reattach from another terminal
decompose up -d
decompose attach

# Ctrl-C detaches without stopping the daemon
```

### Environment files

```bash
# Load an extra env file
decompose -e secrets.env up -d

# Skip automatic .env loading
decompose --disable-dotenv up -d
```

## Output modes

- `--json`: machine-readable
- `--table`: human-friendly
- default:
  - `table` when stdout is a TTY
  - `table` when `LLM=true` or `CI=true`
  - otherwise `json`

## Runtime model

- Per-environment daemon, isolated by working directory + config path hash.
- Local socket IPC via [`interprocess`](https://docs.rs/interprocess/latest/interprocess/local_socket/index.html).
- XDG-aware paths:
  - socket: `$XDG_RUNTIME_DIR/decompose/<instance>.sock` (fallbacks applied)
  - state: `$XDG_STATE_HOME/decompose/<instance>.pid` and `.log`

## Configuration reference

### Config file discovery

If `-f/--file` is omitted, decompose looks for the first matching file in the
current directory:

1. `compose.yml`
2. `compose.yaml`
3. `decompose.yml`
4. `decompose.yaml`

Multiple `-f` flags merge with overlay semantics -- later files override
earlier ones.

### Quick example

```yaml
processes:
  hello:
    command: "echo hello"
  date:
    command: "date"
    depends_on:
      hello:
        condition: process_completed_successfully
```

```bash
decompose up
decompose ps
decompose down
```

### Global settings

These are top-level keys in the YAML file, alongside `processes`.

```yaml
environment:            # Global env vars applied to every process
  SHARED_KEY: value

exit_mode: wait_all     # How the daemon behaves when processes exit

disable_env_expansion: false  # Disable ${VAR} interpolation globally
```

| Field | Type | Default | Description |
|---|---|---|---|
| `environment` | map or list | `{}` | Environment variables applied to all processes. Accepts a YAML map (`KEY: value`) or a list of `KEY=VALUE` strings. |
| `exit_mode` | string | `wait_all` | Controls daemon behavior when processes exit. One of: `wait_all` (keep running until all processes finish or `down` is called), `exit_on_failure` (stop everything if any process exits non-zero), `exit_on_end` (stop everything when any process exits). |
| `disable_env_expansion` | bool | `false` | When `true`, disables `${VAR}` interpolation in all string fields. |
| `processes` | map | *required* | Map of process name to process configuration. At least one process must be defined. |

### Process settings

Each key under `processes` defines a named service.

```yaml
processes:
  web:
    command: "npm start"
    description: "Frontend dev server"
    working_dir: "./frontend"
    environment:
      PORT: "3000"
    env_file:
      - "frontend.env"
    disabled: false
    replicas: 1
    ready_log_line: "Listening on port \\d+"
    restart_policy: on_failure
    backoff_seconds: 2
    max_restarts: 5
```

| Field | Type | Default | Description |
|---|---|---|---|
| `command` | string | *required* | Shell command to run. Executed via the system shell. |
| `description` | string | `null` | Optional human-readable description. |
| `working_dir` | string | config file directory | Working directory for the process. Relative paths resolve from the config file location. |
| `environment` | map or list | `{}` | Per-process environment variables. Same format as the global `environment` (map or list of `KEY=VALUE`). Merged on top of global vars. |
| `env_file` | list of strings | `[]` | Additional `.env` files to load for this process. Paths are relative to the config file directory. |
| `disabled` | bool | `false` | When `true`, the process is visible in `ps` output but not auto-started by `up`. Can be started explicitly with `start`. |
| `replicas` | integer | `1` | Number of instances to run. When greater than 1, instances are named `service[1]`, `service[2]`, etc. Must be at least 1. |
| `ready_log_line` | string (regex) | `null` | A regex pattern matched against process stdout/stderr. When a line matches, the process is marked as "log ready". Required if any other process depends on this one with `process_log_ready` condition. |
| `restart_policy` | string | `no` | Restart behavior: `no` (never restart), `on_failure` (restart on non-zero exit), `always` (restart on any exit). |
| `backoff_seconds` | integer | `1` | Delay in seconds between restart attempts. |
| `max_restarts` | integer or null | `null` | Maximum number of restarts. `null` means unlimited. |

### Dependencies

Use `depends_on` to control startup order. Each dependency names another
process and a condition that must be met before the dependent process starts.

```yaml
processes:
  db:
    command: "postgres -D ./data"
    readiness_probe:
      exec:
        command: "pg_isready"

  api:
    command: "cargo run"
    ready_log_line: "Listening on 0.0.0.0:8080"
    depends_on:
      db:
        condition: process_healthy

  web:
    command: "npm start"
    depends_on:
      api:
        condition: process_log_ready
```

| Condition | Description |
|---|---|
| `process_started` | The dependency has been started (default if omitted). |
| `process_completed` | The dependency has exited (any exit code). |
| `process_completed_successfully` | The dependency has exited with code 0. |
| `process_healthy` | The dependency's readiness probe is passing. Requires `readiness_probe` to be configured on the dependency. |
| `process_log_ready` | The dependency's `ready_log_line` regex has matched. Requires `ready_log_line` to be configured on the dependency. |

Circular dependencies are detected at config load time and produce an error.

### Health probes

Both `readiness_probe` and `liveness_probe` share the same schema. The
readiness probe sets the process's "healthy" flag (used by
`process_healthy` dependency condition). The liveness probe restarts the
process if it fails.

Each probe supports one check type: `exec` (run a command) or `http_get`
(make an HTTP request).

```yaml
processes:
  api:
    command: "cargo run"
    readiness_probe:
      exec:
        command: "curl -sf http://localhost:8080/health"
      period_seconds: 10
      timeout_seconds: 1
      initial_delay_seconds: 5
      success_threshold: 1
      failure_threshold: 3

    liveness_probe:
      http_get:
        host: "127.0.0.1"
        port: 8080
        scheme: http
        path: /healthz
      period_seconds: 30
      failure_threshold: 5
```

**Probe timing fields:**

| Field | Type | Default | Description |
|---|---|---|---|
| `period_seconds` | integer | `10` | How often to run the check. |
| `timeout_seconds` | integer | `1` | Timeout for each check attempt. |
| `initial_delay_seconds` | integer | `0` | Delay before the first check after the process starts. |
| `success_threshold` | integer | `1` | Consecutive successes required to pass. |
| `failure_threshold` | integer | `3` | Consecutive failures required to fail. |

**Exec check:**

| Field | Type | Description |
|---|---|---|
| `exec.command` | string | Shell command to run. Exit code 0 means healthy. |

**HTTP check:**

| Field | Type | Default | Description |
|---|---|---|---|
| `http_get.host` | string | `127.0.0.1` | Host to connect to. |
| `http_get.port` | integer | *required* | Port number. |
| `http_get.scheme` | string | `http` | URL scheme (`http` or `https`). |
| `http_get.path` | string | `/` | Request path. |

### Shutdown configuration

Control how processes are stopped when `decompose down`, `stop`, or `kill`
is called.

```yaml
processes:
  worker:
    command: "python worker.py"
    shutdown:
      command: "python cleanup.py"   # Run before sending signal
      signal: 15                     # Signal number (15 = SIGTERM)
      timeout_seconds: 30            # Wait this long before SIGKILL
```

| Field | Type | Default | Description |
|---|---|---|---|
| `shutdown.command` | string | `null` | Optional command to run before sending the stop signal. |
| `shutdown.signal` | integer | `15` | Signal to send to the process (15 = SIGTERM, 2 = SIGINT, etc.). |
| `shutdown.timeout_seconds` | integer | `10` | Seconds to wait after sending the signal before sending SIGKILL. |

### Environment variables

#### Precedence (lowest to highest)

Environment variables are merged in this order. Later sources override
earlier ones:

1. `.env` file in the config directory (auto-loaded unless `--disable-dotenv`)
2. Explicit env files via `-e` CLI flag
3. Global `environment` block in the YAML
4. Per-process `env_file` entries
5. Per-process `environment` block

#### Variable interpolation

String fields support `${VAR}` substitution from the merged environment.

| Syntax | Description |
|---|---|
| `${VAR}` | Substitute the value of `VAR`. Empty string if unset. |
| `$VAR` | Same as `${VAR}`. |
| `${VAR:-default}` | Substitute `VAR` if set, otherwise use `default`. |
| `$$` | Literal `$` character (escape). |

Interpolation is applied to these fields: `command`, `description`,
`working_dir`, `ready_log_line`, `shutdown.command`, and all environment
variable values.

Disable interpolation globally by setting `disable_env_expansion: true` at
the top level.

#### Environment format

Both map and list formats are accepted anywhere environment variables are
defined:

```yaml
# Map format
environment:
  PORT: "3000"
  DEBUG: "true"

# List format
environment:
  - PORT=3000
  - DEBUG=true
```

## Migrating from Docker Compose

`decompose` is designed to feel familiar to Docker Compose users. If you already have a `docker-compose.yml`, most of it can be adapted with minimal changes.

### What maps directly

These fields work the same way (or very similarly) in both tools:

| Docker Compose field | decompose equivalent | Notes |
|---|---|---|
| `command` | `command` | Runs as a native shell command instead of inside a container |
| `environment` | `environment` | Map or list of `KEY=VALUE` entries |
| `env_file` | `env_file` | Additional `.env` files to load |
| `working_dir` | `working_dir` | Defaults to the config file directory |
| `depends_on` | `depends_on` | Supports conditions: `process_started`, `process_completed`, `process_completed_successfully`, `process_healthy`, `process_log_ready` |
| `healthcheck` | `readiness_probe` / `liveness_probe` | Similar concept, slightly different schema (see below) |
| `restart` | `restart_policy` | Supports `no`, `on_failure`, `always` |
| `deploy.replicas` | `replicas` | Directly on the process definition |
| `stop_grace_period` | `shutdown.timeout_seconds` | Time to wait before SIGKILL |
| `stop_signal` | `shutdown.signal` | Signal number (e.g., `15` for SIGTERM) |

### What doesn't apply

Since decompose runs native processes instead of containers, these Docker Compose fields have no equivalent and should be removed:

- **`image`** -- Use `command` to run the process directly (e.g., `node server.js`, `python app.py`).
- **`build`** -- No container image builds. If you need a build step, add it as a separate process with a dependency.
- **`ports`** -- No port mapping needed; processes bind to host ports directly.
- **`volumes`** -- No mount translation; processes access the host filesystem natively.
- **`networks`** -- No container networking; processes communicate over localhost.
- **`expose`**, **`links`**, **`extra_hosts`** -- Not applicable.
- **`container_name`**, **`hostname`**, **`domainname`** -- Not applicable.
- **`entrypoint`** -- Fold into `command`.
- **`cap_add`**, **`cap_drop`**, **`privileged`**, **`security_opt`** -- Not applicable.

### Config file naming

decompose auto-discovers config files in this order:

1. `compose.yml` (same filename Docker Compose uses)
2. `compose.yaml`
3. `decompose.yml`
4. `decompose.yaml`

You can keep your file named `compose.yml` and decompose will find it, or rename to `decompose.yml` to avoid ambiguity.

### CLI command parity

**Works the same:**

| Command | Notes |
|---|---|
| `up [-d] [SERVICE...]` | Starts services; `-d` detaches |
| `down` | Stops the environment |
| `ps` | Shows process status |
| `logs [-f] [-n N] [SERVICE...]` | View/follow logs |
| `start [SERVICE...]` | Start stopped services |
| `stop [SERVICE...]` | Stop running services |
| `restart [SERVICE...]` | Restart services |

**Not implemented** (container-specific or not applicable):

`build`, `pull`, `push`, `create`, `run`, `exec`, `port`, `top`, `events`, `images`, `pause`, `unpause`, `kill`, `cp`, `wait`

### Health check conversion

Docker Compose:

```yaml
services:
  web:
    healthcheck:
      test: ["CMD", "curl", "-f", "http://localhost:8080/health"]
      interval: 10s
      timeout: 1s
      start_period: 5s
      retries: 3
```

decompose:

```yaml
processes:
  web:
    command: "node server.js"
    readiness_probe:
      exec:
        command: "curl -f http://localhost:8080/health"
      period_seconds: 10
      timeout_seconds: 1
      initial_delay_seconds: 5
      failure_threshold: 3
```

decompose also supports `http_get` probes as an alternative to `exec`:

```yaml
    readiness_probe:
      http_get:
        host: "127.0.0.1"
        port: 8080
        path: /health
        scheme: http
```

### Quick conversion checklist

1. **Rename or copy** your `docker-compose.yml` to `compose.yml` (or `decompose.yml`).
2. **Remove the top-level `services:` key** and replace it with `processes:` (or keep `services:` -- decompose uses `processes:`).
3. **Replace `image:` with `command:`** -- specify the shell command that starts each service (e.g., `python manage.py runserver`, `npm start`).
4. **Remove `build:`**, `ports:`, `volumes:`, `networks:`, and any other container-specific fields.
5. **Keep `environment:`, `env_file:`, `working_dir:`, and `depends_on:`** -- these work as-is.
6. **Convert `healthcheck:` to `readiness_probe:`** using the schema shown above.
7. **Convert `restart:` to `restart_policy:`** -- values `no`, `on-failure`/`on_failure`, and `always` are supported.
8. **Convert `deploy.replicas:` to `replicas:`** at the process level.
9. **Test** with `decompose config` to validate your converted file, then `decompose up`.

### Before and after example

Docker Compose:

```yaml
services:
  api:
    build: .
    ports:
      - "3000:3000"
    environment:
      DATABASE_URL: postgres://localhost/mydb
    depends_on:
      db:
        condition: service_healthy
  db:
    image: postgres:16
    volumes:
      - pgdata:/var/lib/postgresql/data
    healthcheck:
      test: ["CMD", "pg_isready"]
      interval: 5s

volumes:
  pgdata:
```

decompose:

```yaml
processes:
  api:
    command: "npm start"
    environment:
      DATABASE_URL: postgres://localhost/mydb
    depends_on:
      db:
        condition: process_healthy
  db:
    command: "pg_ctl start -D /usr/local/var/postgresql@16 -l db.log"
    readiness_probe:
      exec:
        command: "pg_isready"
      period_seconds: 5
```
