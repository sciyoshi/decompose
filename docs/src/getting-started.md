# Getting Started

This guide walks you through installing `decompose`, writing your first compose
file, and using the core commands to manage local services.

## Installation

The quickest way to install is from crates.io:

```bash
cargo install decompose
```

Prebuilt binaries are also available for Linux and macOS from the
[latest release](https://github.com/sciyoshi/decompose/releases/latest).
See the [README](https://github.com/sciyoshi/decompose#installing) for
additional installation methods including Nix and building from source.

## Your first compose file

Create a file called `decompose.yaml` in your project directory. This example
defines a simple web server and a background worker:

```yaml
processes:
  web:
    command: "python -m http.server 8000"
    description: "Local HTTP file server"

  worker:
    command: "echo 'worker started' && sleep infinity"
    description: "Background task runner"

  logger:
    command: "while true; do echo 'heartbeat'; sleep 5; done"
    description: "Periodic heartbeat logger"
```

Each entry under `processes` defines a service with at least a `command` field.
The `description` is optional but helps when reviewing `decompose ps` output.

## Starting services

Start all services in the background with the `-d` (detach) flag:

```bash
decompose up -d
```

This spawns a daemon that manages the processes. Your terminal returns
immediately so you can continue working. Without `-d`, the output from all
services streams to your terminal and Ctrl-C detaches (the daemon keeps
running).

To start only specific services, pass their names:

```bash
decompose up -d web worker
```

## Checking status

Use `decompose ps` to see what is running:

```bash
decompose ps
```

This prints a table showing each process, its PID, status, and uptime. You can
also get machine-readable output with `--json`:

```bash
decompose ps --json
```

## Viewing logs

Stream logs from all services in real time:

```bash
decompose logs -f
```

To view logs for a specific service:

```bash
decompose logs -f web
```

Use `-n` to control how many historical lines to show (default is 10):

```bash
decompose logs -n 50 worker
```

## Stopping services

Shut down all services and terminate the daemon:

```bash
decompose down
```

To stop individual services without tearing down the entire environment, use
`decompose stop`:

```bash
decompose stop worker
```

Stopped services can be restarted later with `decompose start worker`.

## Adding dependencies

In most projects, services need to start in a specific order. Use `depends_on`
to declare dependencies between processes.

Here is an updated compose file where the worker waits for the web server to
start, and the logger waits for the worker to be ready:

```yaml
processes:
  web:
    command: "python -m http.server 8000"
    ready_log_line: "Serving HTTP"

  worker:
    command: "echo 'worker started' && sleep infinity"
    depends_on:
      web:
        condition: process_log_ready

  logger:
    command: "while true; do echo 'heartbeat'; sleep 5; done"
    depends_on:
      worker:
        condition: process_started
```

The `ready_log_line` field accepts a regex pattern. When the web server prints
a line matching `"Serving HTTP"`, it is marked as log-ready, and the worker is
allowed to start.

Available dependency conditions:

| Condition | Meaning |
|-----------|---------|
| `process_started` | The dependency has been started |
| `process_completed` | The dependency has exited (any exit code) |
| `process_completed_successfully` | The dependency exited with code 0 |
| `process_healthy` | The dependency's readiness probe is passing |
| `process_log_ready` | The dependency matched its `ready_log_line` pattern |

## Environment variables

You can set environment variables globally or per-process:

```yaml
environment:
  SHARED_SECRET: "abc123"

processes:
  web:
    command: "python -m http.server ${PORT}"
    environment:
      PORT: "8000"
```

A `.env` file in the project directory is loaded automatically. Variable
interpolation with `${VAR}` and `${VAR:-default}` works in commands,
descriptions, and environment values. See the
[Configuration](configuration.md) page for the full precedence rules.

## Next steps

- [Configuration](configuration.md) -- full YAML schema reference, environment
  variable precedence, and interpolation rules.
- [Commands](commands.md) -- complete list of CLI commands and flags.
- [Migrating from Docker Compose](migration.md) -- differences and how to
  convert an existing `docker-compose.yml`.
