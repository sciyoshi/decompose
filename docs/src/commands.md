# Commands

`decompose` aims for broad compatibility with the Docker Compose CLI. The
sections below cover every subcommand the binary exposes, the flags they
accept, and the global flags shared by all of them.

## Global flags

These appear *before* the subcommand, matching `docker compose -f FILE <cmd>`.

| Flag | Description |
|------|-------------|
| `-f`, `--file FILE` | Config file path. Repeatable; later files overlay earlier ones. |
| `--session NAME` | Override the project/session name (otherwise derived from the config dir). Also reads `DECOMPOSE_SESSION`. Alias: `--project-name`. |
| `-e`, `--env-file FILE` | Extra `.env` file(s) to load on top of the auto-discovered `.env`. |
| `--disable-dotenv` | Don't auto-load `.env` from the config directory. |
| `--json` / `--table` | Force output format. Without either flag, JSON is used in non-TTY/CI/LLM contexts and a table is used at an interactive terminal. |

## Process lifecycle

### `decompose up [FLAGS] [SERVICE...]`

Start services and (by default) attach to streaming logs until Ctrl-C.

| Flag | Description |
|------|-------------|
| `-d`, `--detach` | Start the daemon and return immediately. |
| `--wait` | With `-d`, wait until every selected service is started/healthy before returning. |
| `--no-deps` | Don't auto-start dependencies of the named services. |
| `--remove-orphans` | Stop and drop services that exist in the daemon but not in the current config. |
| `--force-recreate` | Recreate every service regardless of whether its config hash changed. Conflicts with `--no-recreate`. |
| `--no-recreate` | Keep existing services even if their config hash differs. |
| `--no-start` | Register new/changed services but leave them in `not_started`. |
| `--tui` | Start services and immediately open the TUI. Implies `-d` (services keep running after the TUI exits). |

If no `SERVICE` is given, all services are started.

### `decompose down [FLAGS]`

Stop every running service and shut the daemon down.

| Flag | Description |
|------|-------------|
| `-t`, `--timeout SECONDS` | Override the per-service shutdown timeout before SIGKILL. |

### `decompose start [SERVICE...]`

Start services that are currently in `not_started` or `stopped`. With no
arguments, starts everything.

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

## Inspection

### `decompose ps`

List the current process state — name, base, pid, state glyph (running /
ready / failed / stopped) and replica index where applicable.

### `decompose logs [FLAGS] [SERVICE...]`

Print the daemon log, optionally filtered to a subset of services.

| Flag | Description |
|------|-------------|
| `-f`, `--follow` | Stream new lines as they arrive (Ctrl-C to exit). |
| `-n`, `--tail N` | Show only the last `N` lines of backlog. `-n 0` means start streaming from now (useful with `-f`). |
| `--no-pager` | Don't pipe the one-shot output through `$PAGER` / `less -R`. |

When a single `SERVICE` is given, the `[name] ` prefix is stripped from each
line. Pager honors `DECOMPOSE_PAGER`, then `PAGER`, defaulting to `less -R`.
An empty pager env var disables paging (matches git's convention).

### `decompose attach`

Reattach to a detached session's log stream until Ctrl-C. Doesn't change
process state — the daemon keeps running on disconnect.

### `decompose tui`

Open the interactive terminal UI against a running environment. Shows
process state, lets you tail logs per service, and search across them. The
daemon and its services are unaffected by the TUI exiting; press `Q` (or
Ctrl-C) to leave. See [Configuration](configuration.md) for keybindings.

### `decompose config`

Validate and print the fully-resolved configuration (after merge,
interpolation, and overlay) without starting anything.

### `decompose ls`

List every running decompose environment on the machine — instance ID,
session name, daemon pid, and config directory.

## Ad-hoc commands

### `decompose run [FLAGS] SERVICE COMMAND...`

Run a one-off command using the named service's environment (working dir,
`environment`, `env_file`). Does **not** require a running daemon, does not
attach to a running replica, and is not added to the supervised process
list — fire-and-forget.

| Flag | Description |
|------|-------------|
| `-w`, `--workdir DIR` | Override the service's working directory for this command. |
| `--env KEY=VALUE` | Extra environment variable. Repeatable; overrides values from the service environment. |

Example: `decompose run web bundle exec rails console`.

### `decompose exec [FLAGS] SERVICE COMMAND...`

Like `run`, but requires the daemon to be up *and* at least one replica of
`SERVICE` to be in the `running` state. Useful when you want a one-off
command to see the same env mutations the supervisor applied.

| Flag | Description |
|------|-------------|
| `-w`, `--workdir DIR` | Override the working directory. |
| `--env KEY=VALUE` | Extra environment variable. Repeatable. |

## Shell integration

### `decompose completion SHELL`

Emit a shell completion script for the given shell to stdout. Supported
shells: `bash`, `zsh`, `fish`, `powershell`, `elvish`. See
[Shell completion](completion.md) for installation snippets.
