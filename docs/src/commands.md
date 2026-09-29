# Commands

`decompose` aims for broad compatibility with the Docker Compose CLI. The
sections below cover every subcommand the binary exposes, the flags they
accept, and the global flags shared by all of them.

## Global flags

These appear *before* the subcommand, matching `docker compose -f FILE <cmd>`.

| Flag | Description |
|------|-------------|
| `-f`, `--file FILE` | Config file path. Repeatable; later files overlay earlier ones. |
| `--session NAME` | Use a project/session name or a full instance ID from `ls` (otherwise derived from the config dir). Also reads `DECOMPOSE_SESSION`. Alias: `--project-name`. |
| `-e`, `--env-file FILE` | Extra `.env` file(s) to load on top of the auto-discovered `.env`. |
| `--disable-dotenv` | Don't auto-load `.env` from the config directory. |
| `--json` / `--table` | Select JSON or text output explicitly. Text is always the default, including in pipes and CI; the flags conflict. |

See [Output and scripting](output.md) for JSON results, log streams, diagnostics,
and automation examples.

## Process lifecycle

### `decompose up [FLAGS] [SERVICE...]`

Start services and (by default) attach to streaming logs until Ctrl-C.

| Flag | Description |
|------|-------------|
| `-d`, `--detach` | Start the daemon and return immediately. |
| `--wait` | With `-d`, wait until every selected service is initialized and started/healthy before returning. Requires `-d`/`--detach`. |
| `--no-deps` | Omit dependency expansion when launching a new daemon; see the existing-daemon limitation below. |
| `--remove-orphans` | Stop and drop services that exist in the daemon but not in the current config. |
| `--force-recreate` | Recreate every service regardless of whether its config hash changed. Conflicts with `--no-recreate`. |
| `--no-recreate` | Keep existing services even if their config hash differs. |
| `--no-start` | Register new/changed services but leave them in `not_started`. |
| `--tui` | Start services and immediately open the TUI. Implies `-d` (services keep running after the TUI exits). |

If no `SERVICE` is given, all eligible services are started; disabled services
remain parked. On an existing daemon, `up` reloads the whole project even
when service names are supplied, and `--no-deps` does not suppress dependency
expansion by the subsequent start request. See [Managing a running project](managing-projects.md)
for service selection, reloads, and environment ownership.

### `decompose down [FLAGS]`

Stop every running service and shut the daemon down.

| Flag | Description |
|------|-------------|
| `-t`, `--timeout SECONDS` | Override each service's grace period, including its shutdown command, before SIGKILL. |

Services stop in dependency order (dependents first). Ctrl-C while waiting
requests forced shutdown. See [Shutdown configuration](configuration.md#shutdown-configuration)
for the sequence and timeout behavior.

### `decompose start [SERVICE...]`

Start services that have not started, stopped, exited, or failed to start,
using their stored configuration and also starting dependencies. With no arguments, starts all eligible services; name
a disabled service explicitly to enable it. This command does not reload files.

### `decompose stop [SERVICE...]`

Stop running services. With no arguments, stops everything (the daemon
keeps running). Use `down` if you also want to stop the daemon.

### `decompose restart [SERVICE...]`

Stop, then start the listed services. With no arguments, restarts all.

### `decompose kill [FLAGS] [SERVICE...]`

Send a signal directly to running services (skips the configured
`shutdown.command` and timeout).

| Flag | Description |
|------|-------------|
| `-s`, `--signal SIGNAL` | Signal name (`SIGTERM`, `TERM`, `USR1`) or number (`9`, `15`). Defaults to `SIGKILL`. |

### Startup initialization and waiting

`up` and `start` run pre-start and post-start hooks for new/stopped replicas;
`restart` reevaluates both phases. Repeated `up`/`start` on an unchanged running
replica does not rerun hooks. Scale-up runs them only for new replicas; scale-down
and recreation cancel and clean up affected hook processes.

`up -d` acknowledges launch asynchronously. `up -d --wait` also requires hook
success before applying its usual started/healthy criterion. Hook failures fail
waiting promptly with the service, phase, hook, and cause. Successfully exited
one-shot jobs use the latest attempt's historical initialization result. A job
that exits before post-start completes has not initialized successfully.

The overall CLI wait deadline is controlled by
`DECOMPOSE_DAEMON_READY_TIMEOUT_MS` (default five minutes). On expiry it reports
the active hook or readiness condition; it does not cancel detached work.
Individual hook deadlines remain independent. `run` and `exec` never execute
startup hooks for their one-off commands. There is no skip-hooks or cache-reset
command: restart to retry, and intentionally change a guard or its owned data
when guarded work should run again.

## Inspection

### `decompose ps`

List process names, state glyphs and labels, PIDs, and failure details. Replicated
services include a suffix such as `worker[1]` in their names. JSON output also
includes separate `base` and `replica` fields.

### `decompose logs [FLAGS] [SERVICE...]`

Print retained service output, hook output, and lifecycle records, optionally
filtered to services. Without a service filter, daemon output is included too.
The command requires a running, reachable daemon.

| Flag | Description |
|------|-------------|
| `-f`, `--follow` | Stream new lines as they arrive (Ctrl-C to exit). |
| `-n`, `--tail N` | Limit the combined backlog to the last `N` records, across selected services. Without it, read all retained backlog. `-n 0` skips backlog (useful with `-f`). |
| `--no-pager` | Don't pipe the one-shot output through `$PAGER` / `less -R`. |

When a single `SERVICE` is given, the `[name] ` prefix is stripped from each
line. Pager honors `DECOMPOSE_PAGER`, then `PAGER`, defaulting to `less -R`.
An empty pager env var disables paging (matches git's convention).
See [troubleshooting](troubleshooting.md#where-logs-live-and-how-long-they-last)
for log locations and retention.

### `decompose attach`

Reattach to a detached session's log stream until Ctrl-C. Doesn't change
process state — the daemon keeps running on disconnect.

### `decompose tui`

Open the interactive terminal UI against a running environment. Shows
process state and combined logs, with search and service controls. Press
`q` or Ctrl-C to detach and leave services running; uppercase `Q` stops the
environment before exiting. See the [terminal UI reference](tui.md) for
keybindings and shutdown behavior.

### `decompose config`

Validate and print the resolved configuration after merge and interpolation
without starting anything.

### `decompose ls`

List decompose environments discovered in the runtime socket directory,
showing each instance ID, daemon state, total process count (including stopped
processes and replicas), project directory, and config files. Config files are
shown relative to the project directory where possible. `--json` includes
`project_dir`, `config_files` (full paths), and `process_count` on each environment.
Older daemons do not report these details: the table shows `-` and JSON uses
`null` until the environment is restarted with the updated binary.

## Ad-hoc commands

### `decompose run [FLAGS] SERVICE COMMAND...`

Run a one-off command using the named service's configured working directory
and environment. No running daemon is required. The command runs locally,
outside the supervised process list, and does not execute startup hooks.
Decompose connects it to your terminal, waits for it to finish, and returns
its exit code (128 + signal if terminated by a signal on Unix).

Configuration and env files are resolved by this CLI invocation. Interpolation
therefore sees your current shell environment, which may differ from the
environment inherited by an already-running daemon. The child receives only
the resolved service environment and explicit `--env` overrides; it does not
inherit unrelated shell variables.

| Flag | Description |
|------|-------------|
| `-w`, `--workdir DIR` | Override the service's working directory for this command. |
| `--env KEY=VALUE` | Extra environment variable. Repeatable; overrides values from the service environment. |

Put decompose flags before `SERVICE`: everything after the service name is
the command and its arguments. Decompose executes them directly, without
adding a shell. For pipes or expansion inside the child, invoke a shell
explicitly:

```sh
decompose run web bundle exec rails console
decompose run --env MESSAGE=hello web sh -c 'printf "%s\\n" "$MESSAGE"'
```

### `decompose exec [FLAGS] SERVICE COMMAND...`

Like `run`, but requires the daemon to be up *and* at least one replica of
`SERVICE` to be in the `running` state. This is a precondition check: the
command still runs locally with freshly resolved configuration, waits for
completion, and returns the child's exit code. It does not enter a running
replica, retrieve that replica's environment, or execute startup hooks.

| Flag | Description |
|------|-------------|
| `-w`, `--workdir DIR` | Override the working directory. |
| `--env KEY=VALUE` | Extra environment variable. Repeatable. |

## Shell integration

### `decompose completion SHELL`

Emit a shell completion script for the given shell to stdout. Supported
shells: `bash`, `zsh`, `fish`, `powershell`, `elvish`. See
[Shell completion](completion.md) for installation snippets.
