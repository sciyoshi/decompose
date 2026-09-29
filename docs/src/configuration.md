# Configuration Reference

## Minimal example

```yaml
processes:
  hello:
    command: "echo hello world"
```

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

## Full YAML schema

```yaml
include: []

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
    is_dotenv_disabled: false
    disabled: false
    replicas: 1
    ready_log_line: "Listening on port \\d+"
    restart_policy: on_failure
    backoff_seconds: 2
    max_restarts: 5

    pre_start:
      - name: prepare
        command: "./prepare-data"
        creates: "./data/.prepared"
    post_start:
      - name: initialize
        command: "./initialize-service"

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
| `include` | list | `[]` | Paths or `{path, processes?}` entries importing reusable processes and global environment. See [Includes and packaged fragments](fragments.md). |
| `environment` | map or list | `{}` | Environment variables applied to every process. Accepts a YAML map (`KEY: value`) or a list of `KEY=VALUE` strings. |
| `exit_mode` | string | `wait_all` | Controls daemon behavior when processes exit. See [exit modes](#exit-modes) below. |
| `disable_env_expansion` | bool | `false` | When `true`, disables content interpolation. Include paths still expand. |
| `processes` | map | `{}` | Process definitions and partial overrides. At least one process must exist after includes and overlays are merged. |

### Exit modes

| Value | Behavior |
|---|---|
| `wait_all` | Keep the daemon running, even after all processes finish. This is the default. |
| `exit_on_failure` | Stop all processes and shut down the daemon when a process reaches a failed terminal state (non-zero exit, signal termination, or failure to start), or a startup hook fails. |
| `exit_on_end` | Stop all processes and shut down the daemon when a service's main process reaches an exited state, regardless of exit code. Hook completion or failure alone does not trigger shutdown. |

With `wait_all`, completed services remain visible in `decompose ps`, and the
daemon stays available for commands such as `decompose start`. Run
`decompose down` when you are finished with the environment.

Automatic restart policies are applied before a main-process exit becomes a
terminal state: while a service is restarting, that exit does not trigger
`exit_on_failure` or `exit_on_end`. An intentional `decompose stop` marks a
service as stopped rather than exited, so it does not trigger these exit modes.

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
| `command` | string | **required after merging** | Shell command to run. Executed via the system shell (`sh -c`). Must not be empty. |
| `description` | string | `null` | Optional human-readable description shown in `ps` output. |
| `working_dir` | string | project directory | Working directory for the process. Relative paths resolve from the first root config directory, including in fragments. |
| `environment` | map or list | `{}` | Per-process environment variables. Same format as the global `environment` field. Merged on top of global vars. |
| `env_file` | list of strings | `[]` | Additional `.env` files to load for this process. Relative paths use the project directory; `${DECOMPOSE_FILE_DIR}` addresses fragment assets. |
| `is_dotenv_disabled` | bool | `false` | Remove keys supplied only by root `.env` / CLI env files from this service's child environment. See [Environment and interpolation](environment.md#the-child-environment). |
| `pre_start` | list | `[]` | Ordered hooks before the main process starts. See [Startup hooks](startup-hooks.md). |
| `post_start` | list | `[]` | Ordered hooks alongside the running process. See [Startup hooks](startup-hooks.md). |
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
| `process_initialized` | Every current replica is running and its startup hooks have succeeded or skipped as already satisfied. |
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
| `http_get.scheme` | string | `http` | Use `http`; the built-in probe only supports plain HTTP. |
| `http_get.path` | string | `/` | Request path. |

An HTTP check succeeds when the response status code is 200–399. Redirect
responses count as success; the probe does not follow redirects.

Although configuration parsing accepts `scheme: https`, the built-in probe
does not establish TLS. For an HTTPS endpoint, use an `exec` probe instead:

```yaml
readiness_probe:
  exec:
    command: "curl --fail --silent --show-error https://localhost:8443/health"
```

---

## Shutdown configuration

Control graceful shutdown for `decompose down`, `stop`, and `restart`, and
when `up` recreates a service. `decompose kill` sends a signal directly and
bypasses this sequence.

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
| `shutdown.signal` | integer | `15` | Signal number to send to the process group. Common values: `15` (SIGTERM), `2` (SIGINT), `9` (SIGKILL). |
| `shutdown.timeout_seconds` | integer | `10` | Total grace period in seconds for the shutdown command and process-group exit, before SIGKILL. |

The shutdown sequence is:

1. Start the per-service `timeout_seconds` deadline.
2. Run `shutdown.command` (if set). If it reaches the deadline, kill the
   command's process group and continue cleanup.
3. Send the configured signal to the service's process group.
4. Wait for the group to exit using only the time remaining before the
   deadline, then send SIGKILL if needed.

The timeout includes the shutdown command: a command that takes 20 seconds
with a 30-second timeout leaves about 10 seconds for the service to exit.
Cleanup also covers descendants that remain in the process group after its
main process exits. After SIGKILL, decompose allows up to five additional
seconds to confirm the group has exited before reporting a cleanup failure.

Among services included in the same stop operation, dependents stop before
their dependencies. Stopping one named service does not automatically stop
its dependents. `down --timeout SECONDS` overrides each service's grace
period; it is not a deadline for the entire environment.

Pressing Ctrl-C while `down` waits requests forced shutdown: remaining
shutdown commands and grace periods are skipped or interrupted, and
dependency ordering no longer delays stopping services.

---

## Environment variables

See [Environment and interpolation](environment.md) for variable precedence,
`${VAR}` versus `$${VAR}`, env file formats, and the difference between a
supervised service and a one-off command. The guide also explains when env
file edits take effect and why changing your shell environment requires
restarting the daemon.

---

## Includes and packaged fragments

Import reusable services with a top-level `include` list. See
[Includes and packaged fragments](fragments.md) for merge rules, process
selection, path anchors, provenance, and packaging fragments with Nix.

## Configuration merging

When multiple config files are provided via `-f` flags, they are merged in
order. The merge rules are:

- **Scalar fields** (`exit_mode`, `disable_env_expansion`): the later file's
  value replaces the earlier one.
- **Global `environment`**: maps are merged key-by-key; later values override
  earlier values for the same key.
- **`processes`**: definitions merge by process name; new names are added.
  Omitted fields inherit. Explicit scalar values replace, including `false`
  and `replicas: 1`. `environment` and `depends_on` merge by key. Lists replace
  when supplied, including empty lists. Probes and `shutdown` replace as whole
  blocks. Optional fields can be cleared with `null` where their schema allows it.
- **Completeness**: `command` is required only after all files are merged.
  An overlay can contain `api: {depends_on: {db: {}}}` or override one environment
  variable without copying the command. Dependency validation also runs against
  the complete project.

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
- Missing or empty `command` on any process
- Missing include files, include conflicts, cycles, or nesting beyond 32 edges
- `replicas` set to 0
- `depends_on` referencing an unknown process name
- `process_log_ready` condition on a dependency that has no `ready_log_line`
- Circular dependencies in the `depends_on` graph

Use `decompose config` to validate and inspect the resolved configuration
after merge and interpolation without starting any processes.

## Startup hooks

Use ordered `pre_start` and `post_start` lists to initialize each service replica.
See [Startup hooks](startup-hooks.md) for the field reference, guards, dependency
conditions, failure handling, and logs.
