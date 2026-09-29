# Troubleshooting

Run inspection commands with the same `--file` and `--session` settings used
to start the project. Replace `api` and `db` below with your service names.

```sh
decompose ps
decompose --json ps
decompose logs --no-pager -n 100 api
decompose config
```

`ps` describes the running daemon; `config` validates and resolves files in
your current shell. They can differ until you apply changes with `up`.

## A service stays pending or initializing

`pending` means the service has not yet satisfied its start conditions.
Inspect its `depends_on` configuration and each dependency with `ps` and
`logs`. For example, a `process_completed_successfully` dependency must exit
with code zero; a `process_healthy` dependency must pass its readiness probe.
Starting the dependency is not enough to satisfy either condition.

`initializing` means pre-start work is in progress, before the main process
has a PID. The status initialization detail identifies the active hook and
stage. JSON status also includes `initialization` and
`initialization_blockers`; the latter explains dependencies waiting for
`process_initialized`. Inspect the named dependency's hooks and logs:

```sh
decompose --json ps
decompose logs --no-pager -n 100 db
decompose logs -f -n 0 db
```

Fix the underlying dependency or hook condition. After editing configuration,
use `decompose up -d --wait` to reload and wait for initialization/readiness.
Use `decompose restart db` to retry unchanged stored configuration. A service
marked `disabled` or `not_started` is parked rather than blocked; explicitly
start it with `decompose start db` when intended.

## A running service never becomes ready

Check service output and the configured probe's command, address, port, path,
timeout, and initial delay. A process can be running while its readiness probe
fails. If a dependent service uses `process_log_ready`, check that
`ready_log_line` matches the main process's output; hook output does not
satisfy it.

`up -d --wait` reports pending initialization or readiness on timeout. Its
overall deadline defaults to five minutes and can be set with
`DECOMPOSE_DAEMON_READY_TIMEOUT_MS`. A timeout leaves detached work running;
inspect it with `ps` and `logs` before retrying. Increasing the deadline helps
slow startup, but does not repair a failing probe. See
[health probes](configuration.md#health-probes) for probe settings.

## A startup hook fails

Status and wait errors identify the service, phase, hook, and failure.
Service logs include hook attribution such as `[pre_start:migrate]`.
Pre-start failure prevents the main process from launching; post-start
failure can leave it running with failed initialization under `wait_all`.
Check the hook's output, working directory, environment, guard, and deadline.

Fix files/configuration and run `up -d --wait`, or use `restart SERVICE` to
retry the stored definition after fixing an external condition. Hook failure
itself does not trigger automatic restarts. Repeating `up` on an unchanged
running process does not rerun hooks; restart that service to retry a failed
post-start hook. See [startup hooks](configuration.md#startup-hooks).

## Changed environment values do not take effect

`start` and `restart` reuse stored configuration. `up` rereads configuration
and env files, but the daemon retains its original inherited shell variables
and launch settings. `config` can therefore show a new value that the daemon
does not use. Follow [refreshing variables](environment.md#refreshing-variables-in-a-running-project)
to choose between `up` and a full `down` followed by `up`.

## The CLI cannot reach the daemon

Use `decompose ps` for the targeted project and `decompose --json ls` to
inspect discovered instances. `daemon not running` means no live daemon was
found for that identity; `up -d` starts one. A connection error or an
`unreachable` discovery entry is different: it is not evidence that services
have stopped.

Check the project/session identity and whether the terminal uses the same
`HOME`, `XDG_RUNTIME_DIR`, and `XDG_STATE_HOME` as the launch shell. Inspect
the daemon diagnostic file below, especially after a failed launch. For a
socket-path-length error, choose a shorter `XDG_RUNTIME_DIR` before launching
and use it consistently for subsequent commands. Avoid deleting sockets or
PID files while a daemon may still be alive. When reachable, `decompose down`
provides the normal controlled shutdown.

## Where logs live and how long they last

`decompose logs` reads retained service stdout/stderr, hook output, and
lifecycle records. With no service filter it also includes daemon output.
It requires a reachable daemon; after shutdown, inspect saved files directly
if needed, before starting a replacement daemon.

The state directory is `$XDG_STATE_HOME/decompose` when set, otherwise
`$HOME/.local/state/decompose`. The instance ID is the 16-character value in
`decompose --json ps` (`daemon.instance`) or `decompose --json ls`.

| File beneath the state directory | Contents and retention |
|---|---|
| `<instance>.log` | Daemon stdout/stderr and diagnostics. Truncated at each new daemon launch; no size-based rotation. |
| `<instance>/logs/*.jsonl` | Per-service, per-replica structured records, including hooks and lifecycle events. Up to four files of 10 MiB per service/replica; older generations are removed as output rotates. |
| `<instance>/logs/*.meta.json` | Maps log file keys to service and replica identities. |

Service logs survive individual service restarts and daemon shutdown, but a
new daemon for the same instance clears the structured log directory. Export
anything needed before restarting the daemon; these files are not a permanent
archive. `logs -n 100` limits the combined retained backlog, not each service
individually. Without `-n`, all retained backlog is read; `logs -f -n 0`
skips the backlog and follows new output.

Sockets are separate: `$XDG_RUNTIME_DIR/decompose` when set, otherwise
`$XDG_STATE_HOME/decompose`, otherwise `$HOME/.local/decompose`. See
[Output and scripting](output.md) for exporting JSON Lines and diagnostics.
