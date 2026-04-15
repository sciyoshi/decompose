# decompose

**Run your stack at native speed.**

`decompose` is a Rust process orchestrator for local development and agentic coding loops. It is broadly compatible with Docker Compose CLI and YAML semantics, but runs native processes instead of containers.

No image builds. No container cold starts. No bridge-network translation overhead. Just your real processes, fast, with a familiar compose-like interface.

## Features

- **Familiar CLI** — Commands like `up`, `down`, `ps`, `logs`, `restart` work just like Docker Compose.
- **Native processes** — No containers, no overhead. Processes run directly on your machine.
- **Daemon architecture** — A background daemon per project manages process lifecycles and communicates via local socket IPC.
- **Dependency management** — Define startup order with `depends_on` conditions including health checks and log readiness.
- **Health probes** — Readiness and liveness probes using exec commands or HTTP checks.
- **Environment variable handling** — `.env` files, per-process overrides, and variable interpolation with fallback defaults.

## Quick links

- [Getting Started](getting-started.md) — Install and run your first project.
- [Configuration](configuration.md) — Full YAML schema reference.
- [Commands](commands.md) — CLI command reference.
- [Migrating from Docker Compose](migration.md) — Guide for Docker Compose users.
