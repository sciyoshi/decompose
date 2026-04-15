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
