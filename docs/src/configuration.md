# Configuration

`decompose` discovers config files in the current directory in this order: `compose.yml`, `compose.yaml`, `decompose.yml`, `decompose.yaml`. You can also specify files explicitly with the `-f` flag. Multiple `-f` flags merge with overlay semantics (later files override earlier ones).

## YAML schema overview

```yaml
# Global settings
environment:           # Global env vars (map or list of KEY=VALUE)
exit_mode: wait_all    # wait_all | exit_on_failure | exit_on_end
disable_env_expansion: false

processes:
  service_name:
    command: "..."                    # Required. Shell command to run.
    description: "..."               # Optional description.
    working_dir: "/path"             # Defaults to config directory.
    environment:                     # Per-process env vars (map or list).
      KEY: value
    env_file: ["extra.env"]          # Additional .env files to load.
    disabled: false                  # Visible but not auto-started.
    replicas: 1                      # Number of instances.
    ready_log_line: "regex pattern"  # Sets log_ready when matched in output.
    restart_policy: "no"             # no | on_failure | always
    backoff_seconds: 1               # Delay between restart attempts.
    max_restarts: null               # Cap on restart count (null = unlimited).

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

## Environment variable precedence

From lowest to highest priority:

1. `.env` file (auto-loaded unless `--disable-dotenv`)
2. Explicit `-e` env files
3. Global `environment` block
4. Per-process `env_file` entries
5. Per-process `environment` block

## Variable interpolation

- `${VAR}` and `$VAR` — substitute from env
- `${VAR:-default}` — substitute with fallback
- `$$` — literal `$` escape
- Disabled globally with `disable_env_expansion: true`

Interpolation is applied to: `command`, `description`, `working_dir`, `ready_log_line`, `shutdown.command`, and environment values.

Full documentation coming soon.
