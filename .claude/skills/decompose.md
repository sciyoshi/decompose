---
name: decompose
description: Manage local development environments with decompose. Use when the user wants to start/stop services, view logs, debug process issues, or work with compose files.
---

You are helping the user manage their local development environment using `decompose`, a process orchestrator that is broadly compatible with Docker Compose but runs native processes.

# Quick reference

```bash
decompose up -d                    # Start all services detached
decompose up -d SERVICE...         # Start specific services (+ deps)
decompose up -d --no-deps SERVICE  # Start without pulling in deps
decompose down                     # Stop everything, tear down daemon
decompose ps                       # Show process status
decompose ps --json                # Machine-readable status
decompose logs -f                  # Follow all logs
decompose logs -f SERVICE          # Follow specific service logs
decompose logs -n 100 SERVICE      # Last 100 lines of a service
decompose start SERVICE            # Start a stopped service
decompose stop SERVICE             # Stop a running service
decompose restart SERVICE          # Restart a service
decompose attach                   # Reattach to daemon log stream
```

# How decompose works

- `decompose up` spawns a **background daemon** that manages all processes. The CLI is a thin client that talks to the daemon over a local Unix socket.
- Each environment is identified by a hash of the config directory + files, or by `--session NAME`.
- Config is discovered automatically: `decompose.yml` > `decompose.yaml` > `compose.yml` > `compose.yaml`. Use `-f FILE` to override (can be repeated for overlay merging).
- Output modes: `--json` (machine), `--table` (human), or auto-detect (TTY/CI/LLM -> table, pipe -> JSON).

# When the user asks to start/debug their environment

1. **Check if a compose file exists** in the working directory. Look for `decompose.yml`, `decompose.yaml`, `compose.yml`, or `compose.yaml`.
2. **Check if a daemon is already running**: `decompose ps --json`. If it returns `"running": true`, services are already up.
3. **Start services**: `decompose up -d` for detached mode.
4. **Check status**: `decompose ps` to verify everything is running.
5. **View logs**: `decompose logs -f SERVICE` to debug startup issues.
6. **Look for errors**: Check if processes are in `stopped` or `exited` status. Use logs to find the root cause.

# When the user asks to debug a failing service

1. Run `decompose ps --json` to see which services are in a bad state.
2. Run `decompose logs SERVICE` to see recent output from the failing service.
3. Check the compose file for configuration issues (bad commands, missing env vars, dependency conditions that can't be met).
4. If a service needs to be restarted after a fix: `decompose restart SERVICE`.
5. If the whole environment is wedged: `decompose down && decompose up -d`.

# Compose file quick reference

```yaml
processes:
  web:
    command: "npm start"
    working_dir: "./frontend"
    environment:
      PORT: "3000"
    depends_on:
      api:
        condition: process_healthy
    readiness_probe:
      http_get:
        port: 3000
        path: /
      period_seconds: 5

  api:
    command: "cargo run"
    environment:
      DATABASE_URL: "${DATABASE_URL}"
    ready_log_line: "listening on"
    restart_policy: on_failure

  worker:
    command: "./run-worker.sh"
    depends_on:
      api:
        condition: process_log_ready
    disabled: true  # Won't auto-start, use: decompose start worker
```

Dependency conditions: `process_started`, `process_completed`, `process_completed_successfully`, `process_healthy`, `process_log_ready`.

# Environment variables

Precedence (lowest to highest):
1. `.env` file (auto-loaded)
2. `-e` env files
3. Global `environment` block
4. Per-process `env_file`
5. Per-process `environment` block

Interpolation: `${VAR}`, `${VAR:-default}`, `$$` (literal dollar).
