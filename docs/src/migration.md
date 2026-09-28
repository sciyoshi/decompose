# Migrating from Docker Compose

`decompose` is designed to feel familiar to Docker Compose users. If you already have a Docker Compose workflow, transitioning to `decompose` is straightforward for local development scenarios where you don't need containerization.

## Key differences

- **No containers** — `decompose` runs native processes directly on your host machine. There are no images to build or pull.
- **No networking abstraction** — Services communicate over localhost. There is no bridge network or DNS-based service discovery.
- **Shell commands** — The `command` field runs a shell command directly, rather than specifying a container entrypoint.
- **No volumes or bind mounts** — Processes access the filesystem directly. Use `working_dir` to control the working directory.

## Translating your Compose file

A Docker Compose service like:

```yaml
services:
  api:
    build: .
    ports:
      - "8080:8080"
    environment:
      DATABASE_URL: postgres://localhost/mydb
    depends_on:
      db:
        condition: service_healthy
```

Becomes:

```yaml
processes:
  api:
    command: "cargo run --release"
    environment:
      DATABASE_URL: postgres://localhost/mydb
    depends_on:
      db:
        condition: process_healthy
```

## Condition mapping

| Docker Compose | decompose |
|----------------|-----------|
| `service_started` | `process_started` |
| `service_completed_successfully` | `process_completed_successfully` |
| `service_healthy` | `process_healthy` |

Full documentation coming soon.

## Lifecycle initialization

`pre_start` and `post_start` use ordered hook objects with `name`, `command`,
`working_dir`, and `environment`. They run native shell commands as the daemon's
OS user. Container images, alternate users, and privilege overrides do not apply.
Each hook needs a diagnostic name; exec-form command arrays are unsupported.

Decompose reevaluates guards on **every spawn**, including automatic restarts,
rather than caching successful pre-start steps across a container lifecycle.
`unless`, `creates`, `wait_for`, and `process_initialized` are decompose extensions.
A post-start command should wait for its administrative endpoint using `wait_for`;
spawn alone does not mean the service accepts connections. Keep initialization
independent of readiness, since readiness may require resources the hook creates.

An `unless` wrapper must distinguish absent resources (exit 1) from connection,
authentication, or query errors (exit 2 or higher). Exit 0 means the state exists;
it is required again after mutation to verify success. Use idempotent scripts,
write artifact markers only after successful work, and provide locking for
resources shared by replicas. See [Startup hooks](configuration.md#startup-hooks)
for the schema and lifecycle rules.
