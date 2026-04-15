# Commands

`decompose` aims for broad compatibility with Docker Compose CLI semantics. Below is a summary of the available commands.

## Process lifecycle

| Command | Description |
|---------|-------------|
| `decompose up [-d] [SERVICE...]` | Start services (detached with `-d`) |
| `decompose down` | Stop all services and shut down the daemon |
| `decompose start [SERVICE...]` | Start specific stopped services |
| `decompose stop [SERVICE...]` | Stop specific running services |
| `decompose restart [SERVICE...]` | Restart specific services |
| `decompose kill [SERVICE...]` | Force-kill specific services |

## Inspection

| Command | Description |
|---------|-------------|
| `decompose ps` | List running processes and their status |
| `decompose logs [-f] [-n N] [SERVICE...]` | View process logs (follow with `-f`) |
| `decompose config` | Validate and display the resolved configuration |
| `decompose ls` | List active decompose sessions |

## Global flags

| Flag | Description |
|------|-------------|
| `-f FILE` | Specify config file(s), can be repeated |
| `--session NAME` | Target a specific named session |
| `--json` | Force JSON output |
| `--table` | Force table output |

Full documentation coming soon.
