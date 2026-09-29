# Migrating from Docker Compose

Decompose runs native processes; it does not build images or emulate a container
runtime. Translate the services you need for local development into host commands,
then make their networking, storage, and readiness explicit. A Compose file is
not a drop-in decompose configuration.

## Plan the conversion

| Compose concept | Native equivalent |
|-----------------|-------------------|
| `image`, `build`, and entrypoint | Install the required binaries and dependencies on the host; write the full shell `command`. |
| Container service names and `ports` | Connect to `127.0.0.1` and the port the process actually binds. There is no service DNS or port mapping; choose unused ports. |
| Volumes and bind mounts | Use host paths and `working_dir`. Create required directories and manage permissions yourself. `down` does not remove application data. |
| `healthcheck` | Define a `readiness_probe` and gate dependents with `process_healthy`. Merely starting a PID does not mean it accepts requests. |
| Container environment | Declare child variables explicitly. Interpolation and child environments are separate; see [Environment and interpolation](environment.md). |
| `docker compose exec` | `decompose exec` starts a new host command using locally resolved service configuration, after checking that the service is running. It does not enter a process or retrieve its live environment. |

Host processes share the host's filesystem, network, and OS user. Install databases
and other tools yourself, using your usual package manager or development shell.
Database connection URLs that previously named `db` must point at the database's
actual host address and listening port.

## A runnable conversion

This example needs decompose, Python 3 available as `python3`, and a POSIX shell.
Docker is not required to run the converted project. Start in a new directory:

```sh
mkdir native-demo
cd native-demo
mkdir public
printf 'hello from the host\n' > public/index.html
printf 'DEMO_PORT=8765\n' > .env
cat > check.py <<'PYTHON'
import os
import urllib.request

url = f"http://127.0.0.1:{os.environ['DEMO_PORT']}/"
print(urllib.request.urlopen(url, timeout=5).read().decode())
PYTHON
```

Choose another unused port in `.env` if 8765 is occupied. A small Compose project
might serve this directory and run a check after it becomes healthy:

```yaml
services:
  web:
    image: python:3.12-alpine
    working_dir: /site
    volumes: ["./public:/site:ro"]
    command: python3 -m http.server 8000
    ports: ["127.0.0.1:${DEMO_PORT}:8000"]
    healthcheck:
      test: ["CMD", "python3", "-c", "import urllib.request; urllib.request.urlopen('http://127.0.0.1:8000/')"]
      interval: 1s
      timeout: 2s
      retries: 10
  check:
    image: python:3.12-alpine
    command: python3 -c "import urllib.request; print(urllib.request.urlopen('http://web:8000/').read().decode())"
    depends_on:
      web:
        condition: service_healthy
```

Save this native replacement as `decompose.yml`:

```yaml
processes:
  web:
    command: "python3 -u -m http.server ${DEMO_PORT} --bind 127.0.0.1 --directory public"
    readiness_probe:
      exec:
        command: >-
          python3 -c "import urllib.request;
          urllib.request.urlopen('http://127.0.0.1:${DEMO_PORT}/')"
      period_seconds: 3
      timeout_seconds: 2
  check:
    command: "python3 check.py"
    depends_on:
      web:
        condition: process_healthy
```

The native server binds the chosen host port directly and reads `public` from the
project directory. The check connects through localhost instead of Compose DNS.
Both services now use the installed Python, so pin its version in your development
environment if reproducibility matters.

Validate the configuration, start just the server, and wait for its HTTP probe:

```sh
decompose config
decompose up -d --wait web
decompose ps
decompose run --env "PATH=$PATH" check python3 check.py
decompose logs --tail 20 web
decompose down
```

`run` prints `hello from the host` and returns the check command's exit
status. The explicit `PATH` keeps Python installed by a development shell
available to the one-off command. It runs without starting dependencies or running
lifecycle hooks; the preceding `up --wait web` ensures the server is ready.
Alternatively, `decompose up -d` starts both configured services, running `check`
only after `web` is healthy. The daemon stays running after the check exits.
If startup waiting fails, inspect `ps` and `logs`, then use `down` to clean up.

For config changes and daemon ownership, see
[Managing a running project](managing-projects.md). In particular, an existing
daemon keeps its original shell environment even when a later CLI command has
new variables. For automation and exit codes, see
[Output and scripting](output.md); for failures, see
[Troubleshooting](troubleshooting.md).

## Condition mapping

| Docker Compose | decompose |
|----------------|-----------|
| `service_started` | `process_started` |
| `service_completed_successfully` | `process_completed_successfully` |
| `service_healthy` | `process_healthy` |

These conditions express startup ordering. Configure the corresponding native
probe or successful one-shot command; renaming a condition alone is insufficient.

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
resources shared by replicas. See [Startup hooks](startup-hooks.md)
for the schema and lifecycle rules.
