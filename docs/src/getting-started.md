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

This walkthrough requires Python 3 (`python3 --version`) and an unused local
port 8000. In a new directory, create `decompose.yaml` with this content:

```yaml
processes:
  web:
    command: "python3 -u -m http.server 8000 --bind 127.0.0.1"
    readiness_probe:
      http_get:
        host: "127.0.0.1"
        port: 8000
        path: /
      period_seconds: 2

  worker:
    command: "while true; do echo 'worker heartbeat'; sleep 5; done"
    depends_on:
      web:
        condition: process_healthy
```

Each service has a shell `command`. The web service serves the current directory
on localhost; `-u` makes Python print logs without buffering. If port 8000 is
already occupied, change it in both the command and the probe.

The HTTP readiness probe checks that the server responds. The worker waits for
that probe to pass before it starts. See [dependency conditions](configuration.md#dependencies)
for other ways to order services.

## Starting services

Start the services in the background and wait until they are ready:

```bash
decompose up -d --wait
```

`-d` leaves the daemon running after the command returns. `--wait` keeps this
command open until the web server is healthy and the worker is running. A
successful result looks like this:

```text
all requested services are ready
```

Without `--wait`, detached startup acknowledges the operation before services
are necessarily ready. Without `-d`, output streams to your terminal. If that
foreground command starts a new daemon, Ctrl-C shuts down its services and
daemon. If it attaches to an existing daemon, Ctrl-C only detaches the viewer.

Open <http://127.0.0.1:8000/> to see the directory listing. Use your chosen port
if you changed the configuration.

## Checking status

```bash
decompose ps
```

The table shows service names, states, PIDs, and any failure details. For
this example, a healthy result looks like the following; PIDs vary:

```text
name    state        pid
web     ● healthy    71966
worker  ● healthy    72020
```

A running service without a readiness probe (the worker here) also displays
`healthy`; this does not mean it passed a health check. Optional service
`description` fields are metadata and do not appear in this table.

For machine-readable status, use `decompose ps --json`. Text is the default,
including when piping output. See [Output and scripting](output.md).

## Viewing logs

Show the worker's recent output:

```bash
decompose logs -n 5 worker
```

For example, after one heartbeat (the PID varies):

```text
started (pid 72020)
worker heartbeat
```

Without `-n`, `logs` prints all retained history. To follow new output from all
services, run:

```bash
decompose logs -f
```

Ctrl-C stops the log viewer and leaves the services running.

## Stopping and cleaning up

Stop and start just the worker while keeping the environment available:

```bash
decompose stop worker
decompose start worker
```

When finished, stop all services and terminate the daemon:

```bash
decompose down
```

## Next steps

- [Managing a running project](managing-projects.md) — choose lifecycle
  commands, target an environment, and apply configuration changes.
- [Environment and interpolation](environment.md) — configure child
  environments and understand when shell and file changes take effect.
- [Configuration](configuration.md) — full YAML schema reference.
- [Commands](commands.md) — complete list of CLI commands and flags.
- [Troubleshooting](troubleshooting.md) — diagnose services that fail to start
  or become ready.
- [Migrating from Docker Compose](migration.md) — convert an existing project.
