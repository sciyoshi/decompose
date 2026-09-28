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

## Includes and packaged fragments

A top-level `include` list imports processes and global environment:

```yaml
include:
  - ${FLOX_ENV}/share/decompose/nats.yaml
  - path: ${FLOX_ENV}/share/decompose/temporal.yaml
    processes: [temporal]

processes:
  nats:
    environment:
      NATS_PORT: '4223'
  api:
    command: ./api
    depends_on:
      nats: {condition: process_started}
```

Paths are interpolated and resolved relative to the file containing the
include. Missing files are errors. Includes may nest, with cycle detection
using canonical paths and a maximum depth of 32 edges.

Files merge in include-list order, followed by the including file's local
definitions. Two imports defining the same process are an error unless the
immediate including file also defines that process. A partial local definition
is sufficient: the imports merge in order, then the local fields override them.
An ancestor's definition cannot resolve a conflict inside a nested include.

`processes` selects names from the composed included file and brings their
transitive dependencies. Omit it to import all processes; use `processes: []`
to import only global environment. Unknown selected names are errors. Global
environment is always imported, with later values winning. Dependencies may
also refer to processes supplied elsewhere in the final project.

Only processes and environment are imported. `exit_mode` and
`disable_env_expansion` are controlled by root config files. With multiple
`--file` arguments, each file's include tree is composed before applying the
root-file overlays in argument order.

The first root config determines the project directory and automatic `.env`.
Included files never load their own `.env`, and their paths are not added to
the instance identity. `up` on an existing daemon rereads included files;
changes to effective process configuration participate in normal reloads.

### Path anchors

Two reserved interpolation variables are supplied by decompose:

| Variable | Value | Typical use |
|---|---|---|
| `DECOMPOSE_PROJECT_DIR` | Canonical directory of the first root config | Writable data directories |
| `DECOMPOSE_FILE_DIR` | Canonical directory of the file supplying the value | Assets next to a fragment |

Field origins survive partial overrides. For example, an inherited command
uses the fragment's `DECOMPOSE_FILE_DIR`, while a locally overridden environment
value uses the local file's directory. These are interpolation variables, not
automatically exported child environment variables. Environment declarations
cannot override their meaning during interpolation.

Relative `working_dir` and `env_file` paths use the **project directory**, even
inside fragments. An `env_file` can resolve beneath either the project directory
or the directory of the file that supplied that list. Canonical paths are checked,
so symlinks cannot escape those directories. For a packaged asset, use:

```yaml
env_file: ['${DECOMPOSE_FILE_DIR}/defaults.env']
working_dir: ${DECOMPOSE_PROJECT_DIR}
```

Include-path interpolation uses root `.env` / explicit `--env-file` values,
shell environment, the declaring file's own global environment, and the anchors.
Imported environment does not affect include discovery. Include-path expansion
still runs when `disable_env_expansion: true`; that setting suppresses expansion
of the composed config's content.

This syntax follows Compose's `include` naming, but local conflict overrides,
process selection, root-relative service paths, and root-only dotenv loading
are deliberate differences from the Compose specification.

### Provenance

`decompose config --json` adds a `provenance.processes` map. Each entry contains
`command_source` (the canonical file that supplied the command) and `files`
(contributing files in first-contribution order). Internal field origins also
track environment keys individually. Provenance metadata alone does not change
process hashes or trigger a restart. Table/YAML output contains only the
resolved configuration.

### Building fragments with Nix

Use `decompose.lib.mkFragment` from the decompose flake input:

```nix
natsFragment = decompose.lib.mkFragment {
  inherit pkgs;
  name = "nats";
  src = ./nats.yaml;
  substitutions = { natsServer = pkgs.nats-server; };
};
```

The helper installs `share/decompose/nats.yaml`, replacing `@name@` placeholders
with the supplied values. It preserves decompose interpolation and dollar
escapes. The source fragment can contain:

```yaml
processes:
  nats:
    command: >-
      exec @natsServer@/bin/nats-server --jetstream
      --store_dir "${DECOMPOSE_PROJECT_DIR}/.data/nats"
      --port "$${NATS_PORT:-4222}"
```

Combine fragments into one installable package:

```nix
stack-services = pkgs.symlinkJoin {
  name = "stack-services";
  paths = [ natsFragment temporalFragment ];
};
```

Absolute store paths let services run without their binaries on `PATH` or an
active Flox environment. Nix retains binaries referenced in the output as runtime
dependencies; merely adding a package to build inputs does not retain it. Reference
any required CLI tools in the shipped configuration or assets too. The consumer
can then pin one fragment package, together with its binary closure, in its Flox
manifest lock. The stack repository can include these same installed fragments.

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
| `include` | list | `[]` | Paths or `{path, processes?}` entries importing reusable processes and global environment. |
| `environment` | map or list | `{}` | Environment variables applied to every process. Accepts a YAML map (`KEY: value`) or a list of `KEY=VALUE` strings. |
| `exit_mode` | string | `wait_all` | Controls daemon behavior when processes exit. See [exit modes](#exit-modes) below. |
| `disable_env_expansion` | bool | `false` | When `true`, disables content interpolation. Include paths still expand. |
| `processes` | map | `{}` | Process definitions and partial overrides. At least one process must exist after includes and overlays are merged. |

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
| `command` | string | **required after merging** | Shell command to run. Executed via the system shell (`sh -c`). Must not be empty. |
| `description` | string | `null` | Optional human-readable description shown in `ps` output. |
| `working_dir` | string | project directory | Working directory for the process. Relative paths resolve from the first root config directory, including in fragments. |
| `environment` | map or list | `{}` | Per-process environment variables. Same format as the global `environment` field. Merged on top of global vars. |
| `env_file` | list of strings | `[]` | Additional `.env` files to load for this process. Relative paths use the project directory; `${DECOMPOSE_FILE_DIR}` addresses fragment assets. |
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

String fields support `${VAR}` substitution after merging. For interpolation,
root dotenv values are overridden by the shell environment, then global
`environment`, then per-process `environment`; reserved anchors take precedence.
Per-process `env_file` values are loaded into children, not used for interpolation.
Global environment values are expanded in key order. Process environment values
use a frozen snapshot of their unexpanded values, preserving non-recursive
substitution semantics.

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
- `env_file`
- Probe exec commands and HTTP host, scheme, and path
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

`pre_start` and `post_start` are ordered lists of native shell commands attached
to each service replica. Pre-start runs after dependencies pass and before the
service is spawned. Post-start runs alongside the service and its health probes.
They use the daemon's OS identity, closed stdin, and the service's environment.

```yaml
processes:
  database:
    command: ./run-database
    pre_start:
      - name: prepare
        creates: ./data/.prepared-v1
        command: ./prepare-data
    post_start:
      - name: app-user
        wait_for:
          exec:
            command: ./admin-endpoint-ready
          period_seconds: 1
          timeout_seconds: 2
        unless: ./app-user-exists
        command: ./create-app-user
        timeout_seconds: 60
  application:
    command: ./run-app
    depends_on:
      database:
        condition: process_initialized
```

| Hook field | Default | Meaning |
|------------|---------|---------|
| `name` | required | Literal identifier matching `[A-Za-z0-9][A-Za-z0-9_.-]*`, unique within its phase. |
| `command` | required | Nonempty shell command. |
| `unless` | absent | Resource check: 0 skips, 1 runs the command, every other exit/signal/error fails. |
| `creates` | absent | Existing file or directory skips the command. Mutually exclusive with `unless`. |
| `wait_for` | absent | One-shot prerequisite probe, independent of readiness. |
| `timeout_seconds` | `60` | Positive overall deadline covering wait, check, command, and verification. |
| `working_dir` | service directory | Relative paths resolve against the service directory; it must already exist. |
| `environment` | empty overrides | Map or `KEY=VALUE` list layered on the resolved service environment. |

`wait_for` accepts exactly one `exec: {command: ...}` or
`http_get: {host: ..., port: ..., scheme: http, path: /...}`. It supports
`period_seconds` (1), per-attempt `timeout_seconds` (1), and
`initial_delay_seconds` (0); thresholds and unknown fields are rejected.
The first exec exit 0 or HTTP response in 200–399 passes. Failed attempts retry
within the hook's overall deadline. Spawn errors fail immediately.

An existing `creates` artifact skips even the prerequisite wait. Otherwise,
wait first, then check the guard, execute if absent, and verify once after a
successful command. `unless` must return 0 during verification; `creates` must
exist. A dangling symlink is absent; other filesystem errors fail. Decompose
never writes or deletes markers. Write markers only after initialization succeeds.
Unguarded commands execute every startup and should be idempotent.

Hook environment values expand once against the service environment and hook
overrides. Commands, paths, checks, and probe fields see the resulting environment.
`$$` escapes interpolation; `disable_env_expansion` also applies to hooks. Overrides
do not change the service or later hooks. Relative artifact paths use the hook's
working directory. Each overlay replaces a supplied phase entirely; omission
inherits it, `[]` clears it, and `null` is invalid.
Within hooks, `${DECOMPOSE_FILE_DIR}` refers to the file defining that phase;
`${DECOMPOSE_PROJECT_DIR}` refers to the root project directory. This lets an
included hook reference scripts next to its fragment while a locally replaced
phase references local scripts.

Guards are reevaluated on manual and automatic restarts, recreation, and new
replicas. An unchanged running service does not reevaluate hooks. Hook config
changes trigger recreation; changing a script's contents requires an explicit
restart. There is no persistent success cache or exactly-once guarantee. Guards
are not locks: use idempotent operations or application locks for shared resources,
or use a separate setup service with one owner.

A pre-start failure prevents spawn (`failed_to_start` in the human status;
`failed` in JSON `state`). A post-start failure leaves the service running under
`wait_all`. Hook failures do not trigger automatic restarts, but
`exit_on_failure` stops the project and reports failure. `exit_on_end` responds
only to service-child exits. Restart explicitly to retry failed initialization.

`process_initialized` requires **every current replica** to be running with both
phases successful (or skipped because already satisfied). It resets on exit,
stop, and restart. It is independent of `process_healthy` and the historical
`process_started` condition. Already-running dependents are not restarted when
a dependency loses initialization. Use `process_completed_successfully` for
one-shot jobs and a readiness probe for application-level readiness.

`ps` and the TUI show initialization separately, including failures on healthy
running services and initialization blockers on pending dependents. JSON adds an
`initialization` object with `state`, live `initialized`, active `phase`/`hook`,
and ordered `hooks` records (stage, status, timestamps, exit code, error, reason).
`initialization_blockers` identifies blocking replicas and their records.
Clients should accept the new pre-spawn `initializing` state, which has no PID.
Historical initialization success survives a subsequent service exit.

Hook stdout/stderr use the owning replica's retained logs with `hook_phase`,
`hook_name`, and `hook_stage` metadata. Human logs display `[post_start:app-user]`.
Hook output never satisfies `ready_log_line`. Output is retained as supplied,
just like service output; avoid printing credentials.

Stop, down, restart, child exit, and liveness restarts cancel active hooks and
reap their process groups before another incarnation starts. Timeouts allow up
to five additional seconds of cleanup grace; forced shutdown skips that grace.
Hooks must stay in the foreground. Give liveness probes enough initial delay
if initialization is required for their checks to pass. Do not wait on the
service's own initialization from a hook.
