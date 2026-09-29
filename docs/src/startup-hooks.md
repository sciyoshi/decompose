# Startup hooks

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

A pre-start failure prevents spawn (`failed_to_start` in both human output and
JSON `state`). A post-start failure leaves the service running under `wait_all`,
with JSON `state: "running"`. In either case, `initialization.state` is `failed`
and `initialization.initialized` is `false`. Hook failures do not trigger automatic
restarts, but `exit_on_failure` stops the project and reports failure. `exit_on_end` responds
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
