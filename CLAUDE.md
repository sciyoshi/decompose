# Decompose

A simple process orchestrator for local development. Broadly compatible with
Docker Compose CLI and YAML semantics, but runs native processes instead of
containers.

## Design

- **Daemon architecture**: `decompose up` spawns a background daemon per
  project. The daemon manages process lifecycles and communicates via local
  socket IPC (using the `interprocess` crate). The CLI is a thin client that
  sends JSON requests over the socket.
- **Instance identity**: Each environment is identified by a SHA-256 hash of
  the config directory + file set, or by an explicit `--session` name. This
  allows multiple isolated environments and cross-tab targeting.
- **XDG paths**: Sockets go to `$XDG_RUNTIME_DIR/decompose/`, state (pid, log)
  goes to `$XDG_STATE_HOME/decompose/`, with sensible fallbacks.

## CLI

Goal: broad compatibility with `docker compose` commands. The main commands
should feel familiar to Docker Compose users.

```
decompose [--file FILE...] [--session NAME] [-e ENV_FILE...] [--disable-dotenv] <command>

decompose up [-d] [--no-deps] [SERVICE...]
decompose down
decompose ps
decompose logs [-f] [-n N] [SERVICE...]
decompose start [SERVICE...]
decompose stop [SERVICE...]
decompose restart [SERVICE...]
decompose kill [SERVICE...]
decompose config
decompose ls
```

Global flags (`--file`, `--session`, `-e/--env-file`, `--disable-dotenv`)
appear before the subcommand, matching `docker compose -f FILE <cmd>` shape.

Output modes: `--json`, `--table`, or auto-detect (TTY/CI/LLM -> table,
otherwise JSON).

## Configuration

Config files are discovered in order: `decompose.yml`, `decompose.yaml`,
`compose.yml`, `compose.yaml`. Multiple `-f` flags merge with overlay
semantics (later files override earlier ones).

### YAML schema

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
        # Conditions: process_started, process_completed,
        #   process_completed_successfully, process_healthy,
        #   process_log_ready

    readiness_probe:                 # Sets healthy flag when passing.
      exec:
        command: "curl -f localhost:8080/health"
      period_seconds: 10
      timeout_seconds: 1
      initial_delay_seconds: 0
      success_threshold: 1
      failure_threshold: 3

    liveness_probe:                  # Same schema as readiness_probe.
      http_get:
        host: "127.0.0.1"
        port: 8080
        scheme: http
        path: /

    shutdown:
      command: "cleanup.sh"          # Optional pre-shutdown command.
      signal: 15                     # Signal to send (default SIGTERM).
      timeout_seconds: 10            # Wait before SIGKILL.
```

### Environment variable precedence (lowest to highest)

1. `.env` file (auto-loaded unless `--disable-dotenv`)
2. Explicit `-e` env files
3. Global `environment` block
4. Per-process `env_file` entries
5. Per-process `environment` block

### Variable interpolation

- `${VAR}` and `$VAR` — substitute from env
- `${VAR:-default}` — substitute with fallback
- `$$` — literal `$` escape
- Disabled globally with `disable_env_expansion: true`

Applied to: `command`, `description`, `working_dir`, `ready_log_line`,
`shutdown.command`, and environment values.

## Architecture

```
src/
  main.rs      Entry point, calls run_cli()
  lib.rs       CLI command handlers, log streaming
  cli.rs       Clap argument definitions
  config.rs    YAML parsing, merging, .env loading, interpolation
  daemon.rs    Daemon process lifecycle, supervisor loop, IPC handlers
  model.rs     Core types (ProcessStatus, HealthProbe, etc.)
  ipc.rs       Request/Response protocol, socket helpers
  output.rs    JSON/table output formatting
  paths.rs     XDG path management, instance ID generation
```

## Build, test, lint

All of these must pass before committing:

```bash
cargo build --locked --all-targets
cargo test --locked --all-targets
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
```

## Testing patterns

Integration tests live in `tests/cli_integration.rs` and spawn the compiled
binary end-to-end. Each test creates an isolated temp dir and sets
`XDG_RUNTIME_DIR`, `XDG_STATE_HOME`, and `HOME` to prevent collisions with
real environments. Every test that calls `up` must call `down` before exiting.

Use the existing `setup_project()` and `run_cmd()` helpers when adding tests.

## Adding new CLI commands

1. Add variant to `Commands` enum in `cli.rs` with a doc comment (becomes
   help text). Re-use `GlobalArgs` or `ServiceArgs` where applicable.
2. Add the handler function in `lib.rs`.
3. Wire the match arm in `run_cli()` in `lib.rs`.
4. If the command talks to the daemon, add the request/response variants to
   `ipc.rs` and the handler in `daemon.rs`.
5. Add an integration test in `tests/cli_integration.rs`.

## Commit messages

**Conventional Commits required.** See `AGENTS.md` for the full spec.
Quick reference: `feat(cli):`, `fix(daemon):`, `refactor(config):`,
`test:`, `docs:`, `chore:`.


<!-- BEGIN BEADS INTEGRATION v:1 profile:minimal hash:ca08a54f -->
## Beads Issue Tracker

This project uses **bd (beads)** for issue tracking. Run `bd prime` to see full workflow context and commands.

### Quick Reference

```bash
bd ready              # Find available work
bd show <id>          # View issue details
bd update <id> --claim  # Claim work
bd close <id>         # Complete work
```

### Rules

- Use `bd` for ALL task tracking — do NOT use TodoWrite, TaskCreate, or markdown TODO lists
- Run `bd prime` for detailed command reference and session close protocol
- Use `bd remember` for persistent knowledge — do NOT use MEMORY.md files

## Session Completion

**When ending a work session**, you MUST complete ALL steps below. Work is NOT complete until `git push` succeeds.

**MANDATORY WORKFLOW:**

1. **File issues for remaining work** - Create issues for anything that needs follow-up
2. **Run quality gates** (if code changed) - Tests, linters, builds
3. **Update issue status** - Close finished work, update in-progress items
4. **PUSH TO REMOTE** - This is MANDATORY:
   ```bash
   git pull --rebase
   bd dolt push
   git push
   git status  # MUST show "up to date with origin"
   ```
5. **Clean up** - Clear stashes, prune remote branches
6. **Verify** - All changes committed AND pushed
7. **Hand off** - Provide context for next session

**CRITICAL RULES:**
- Work is NOT complete until `git push` succeeds
- NEVER stop before pushing - that leaves work stranded locally
- NEVER say "ready to push when you are" - YOU must push
- If push fails, resolve and retry until it succeeds
<!-- END BEADS INTEGRATION -->
