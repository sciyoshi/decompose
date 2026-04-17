# Configuration Reference

## Config file discovery

When no `-f`/`--file` flags are given, `decompose` searches the current
directory for the first file that exists, in this order:

1. `decompose.yml`
2. `decompose.yaml`
3. `compose.yml`
4. `compose.yaml`

You can pass one or more `-f` flags to specify config files explicitly.
Multiple files are merged with **overlay semantics** -- fields in later files
override the same fields in earlier files:

```bash
decompose -f base.yml -f dev-overrides.yml up -d
```

## Minimal example

```yaml
processes:
  hello:
    command: "echo hello world"
```

## Full YAML schema

```yaml
environment:
  SHARED_KEY: value

exit_mode: wait_all
disable_env_expansion: false

processes:
  service_name:
    command: "npm start"
    description: "Frontend dev server"
    working_dir: "./frontend"
    environment:
      PORT: "3000"
    env_file:
      - "extra.env"
    disabled: false
    replicas: 1
    ready_log_line: "Listening on port \\d+"
    restart_policy: on_failure
    backoff_seconds: 2
    max_restarts: 5

    depends_on:
      other_service:
        condition: process_started

    readiness_probe:
      exec:
        command: "curl -f localhost:8080/health"
      period_seconds: 10
      timeout_seconds: 1
      initial_delay_seconds: 0
      success_threshold: 1
      failure_threshold: 3

    liveness_probe:
      http_get:
        host: "127.0.0.1"
        port: 8080
        scheme: http
        path: /

    shutdown:
      command: "cleanup.sh"
      signal: 15
      timeout_seconds: 10
```

---

## Global settings

These are top-level keys in the YAML file, alongside `processes`.

| Field | Type | Default | Description |
|---|---|---|---|
| `environment` | map or list | `{}` | Environment variables applied to every process. Accepts a YAML map (`KEY: value`) or a list of `KEY=VALUE` strings. |
| `exit_mode` | string | `wait_all` | Controls daemon behavior when processes exit. See [exit modes](#exit-modes) below. |
| `disable_env_expansion` | bool | `false` | When `true`, disables `${VAR}` interpolation in all string fields. |
| `processes` | map | **required** | Map of process name to [process configuration](#process-settings). At least one process must be defined. |

### Exit modes

| Value | Behavior |
|---|---|
| `wait_all` | Keep the daemon running until all processes finish or `decompose down` is called. This is the default. |
| `exit_on_failure` | Stop all processes and shut down the daemon if any process exits with a non-zero exit code. |
| `exit_on_end` | Stop all processes and shut down the daemon when any process exits, regardless of exit code. |

---

## Process settings

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
| `command` | string | **required** | Shell command to run. Executed via the system shell (`sh -c`). Must not be empty. |
| `description` | string | `null` | Optional human-readable description shown in `ps` output. |
| `working_dir` | string | config file directory | Working directory for the process. Relative paths resolve from the config file location. |
| `environment` | map or list | `{}` | Per-process environment variables. Same format as the global `environment` field. Merged on top of global vars. |
| `env_file` | list of strings | `[]` | Additional `.env` files to load for this process. Paths are relative to the config file directory. |
| `disabled` | bool | `false` | When `true`, the process is visible in `ps` output but not auto-started by `up`. Can be started explicitly with `decompose start`. |
| `replicas` | integer | `1` | Number of instances to run. When greater than 1, instances are named `service[1]`, `service[2]`, etc. Must be at least 1. |
| `ready_log_line` | string (regex) | `null` | A regex pattern matched against process stdout/stderr. When a line matches, the process is marked as "log ready". Required if another process depends on this one with the `process_log_ready` condition. |
| `restart_policy` | string | `no` | Restart behavior when the process exits. See [restart policies](#restart-policies) below. |
| `backoff_seconds` | integer | `1` | Delay in seconds between restart attempts. |
| `max_restarts` | integer or null | `null` | Maximum number of restarts. `null` means unlimited. |
| `depends_on` | map | `{}` | Startup dependencies. See [Dependencies](#dependencies). |
| `readiness_probe` | object | `null` | Health check that sets the "healthy" flag. See [Health probes](#health-probes). |
| `liveness_probe` | object | `null` | Health check that restarts the process on failure. See [Health probes](#health-probes). |
| `shutdown` | object | `null` | Shutdown behavior. See [Shutdown configuration](#shutdown-configuration). |

### Restart policies

| Value | Behavior |
|---|---|
| `no` | Never restart the process after it exits. This is the default. |
| `on_failure` | Restart only if the process exits with a non-zero exit code. |
| `always` | Restart the process whenever it exits, regardless of exit code. |

When a restart policy is active, `backoff_seconds` controls the delay between
attempts and `max_restarts` caps the total number of restarts (set to `null`
for unlimited).

---

## Dependencies

Use `depends_on` to control startup order. Each dependency names another
process and a condition that must be satisfied before the dependent process
starts.

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

### Dependency conditions

| Condition | Description |
|---|---|
| `process_started` | The dependency has been started. This is the default if `condition` is omitted. |
| `process_completed` | The dependency has exited (any exit code). |
| `process_completed_successfully` | The dependency has exited with code 0. |
| `process_healthy` | The dependency's readiness probe is passing. Requires `readiness_probe` on the dependency. |
| `process_log_ready` | The dependency's `ready_log_line` regex has matched. Requires `ready_log_line` on the dependency. |

### Circular dependency detection

Circular dependencies are detected at config load time and produce an error.
For example, if service A depends on B and B depends on A, `decompose` will
refuse to start and report the cycle.

---

## Health probes

Both `readiness_probe` and `liveness_probe` share the same schema. They differ
in effect:

- **Readiness probe** -- Sets the process's "healthy" flag. Used by the
  `process_healthy` dependency condition to gate startup of dependent services.
- **Liveness probe** -- Restarts the process if the probe fails (consecutive
  failures reach `failure_threshold`).

Each probe supports exactly one check type: **exec** (run a shell command) or
**http_get** (make an HTTP request). Do not specify both on the same probe.

### Exec probe example

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
```

### HTTP probe example

```yaml
processes:
  api:
    command: "cargo run"
    liveness_probe:
      http_get:
        host: "127.0.0.1"
        port: 8080
        scheme: http
        path: /healthz
      period_seconds: 30
      failure_threshold: 5
```

### Probe timing fields

| Field | Type | Default | Description |
|---|---|---|---|
| `period_seconds` | integer | `10` | How often (in seconds) to run the check. |
| `timeout_seconds` | integer | `1` | Timeout in seconds for each check attempt. |
| `initial_delay_seconds` | integer | `0` | Seconds to wait after the process starts before running the first check. |
| `success_threshold` | integer | `1` | Number of consecutive successes required to mark the probe as passing. |
| `failure_threshold` | integer | `3` | Number of consecutive failures required to mark the probe as failing. |

### Exec check fields

| Field | Type | Description |
|---|---|---|
| `exec.command` | string | Shell command to run. Exit code 0 means healthy; any other exit code means unhealthy. |

### HTTP check fields

| Field | Type | Default | Description |
|---|---|---|---|
| `http_get.host` | string | `127.0.0.1` | Host to connect to. |
| `http_get.port` | integer | **required** | Port number. |
| `http_get.scheme` | string | `http` | URL scheme. Must be `http` or `https`. |
| `http_get.path` | string | `/` | Request path. |

An HTTP check is considered healthy if the response status code is in the
2xx range.

---

## Shutdown configuration

Control how processes are stopped when `decompose down`, `stop`, or `kill`
is called.

```yaml
processes:
  worker:
    command: "python worker.py"
    shutdown:
      command: "python cleanup.py"
      signal: 15
      timeout_seconds: 30
```

| Field | Type | Default | Description |
|---|---|---|---|
| `shutdown.command` | string | `null` | Optional command to run before sending the stop signal. Useful for graceful cleanup scripts. |
| `shutdown.signal` | integer | `15` | Signal number to send to the process. Common values: `15` (SIGTERM), `2` (SIGINT), `9` (SIGKILL). |
| `shutdown.timeout_seconds` | integer | `10` | Seconds to wait after sending the signal before forcefully killing the process with SIGKILL. |

The shutdown sequence is:

1. Run `shutdown.command` (if set) and wait for it to complete.
2. Send the configured signal to the process.
3. Wait up to `timeout_seconds` for the process to exit.
4. If the process has not exited, send SIGKILL.

---

## Environment variables

### Precedence

Environment variables are merged in the following order. Later sources
override earlier ones:

| Priority | Source | Notes |
|---|---|---|
| 1 (lowest) | `.env` file | Auto-loaded from the config directory unless `--disable-dotenv` is passed. |
| 2 | `-e` CLI flag | Explicit env files passed on the command line. |
| 3 | Global `environment` block | Top-level `environment` in the YAML config. |
| 4 | Per-process `env_file` entries | Files listed in each process's `env_file` array. |
| 5 (highest) | Per-process `environment` block | Inline environment variables on the process definition. |

### Variable interpolation

String fields support `${VAR}` substitution from the merged environment at
the point where the field is evaluated.

| Syntax | Description |
|---|---|
| `${VAR}` | Substitute the value of `VAR`. Empty string if unset. |
| `$VAR` | Same as `${VAR}`. |
| `${VAR:-default}` | Use the value of `VAR` if set; otherwise use `default`. |
| `$$` | Literal `$` character (escape). |

Interpolation is applied to these fields:

- `command`
- `description`
- `working_dir`
- `ready_log_line`
- `shutdown.command`
- All environment variable values (both global and per-process)

Disable interpolation globally by setting `disable_env_expansion: true` at the
top level of the config file.

### Environment format

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

### .env file format

The `.env` file uses simple `KEY=VALUE` lines. Blank lines and lines starting
with `#` are ignored:

```bash
# Database settings
DATABASE_URL=postgres://localhost/mydb
REDIS_URL=redis://localhost:6379

# Feature flags
ENABLE_CACHE=true
```

---

## Configuration merging

When multiple config files are provided via `-f` flags, they are merged in
order. The merge rules are:

- **Scalar fields** (`exit_mode`, `disable_env_expansion`): the later file's
  value replaces the earlier one.
- **Global `environment`**: maps are merged key-by-key; later values override
  earlier values for the same key.
- **`processes`**: if the same process name appears in both files, the later
  definition replaces the earlier one entirely. New process names are added.

This allows you to keep a base configuration and layer environment-specific
overrides on top:

```bash
# base.yml defines all processes
# dev.yml overrides working_dir and environment for local development
decompose -f base.yml -f dev.yml up -d
```

## Validation

`decompose` validates the configuration at load time and reports errors for:

- No processes defined
- Empty `command` on any process
- `replicas` set to 0
- `depends_on` referencing an unknown process name
- `process_log_ready` condition on a dependency that has no `ready_log_line`
- Circular dependencies in the `depends_on` graph

Use `decompose config` to validate and inspect the resolved configuration
without starting any processes.
