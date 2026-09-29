# Output and scripting

Text is always the default, including when stdout is redirected or piped.
The `CI` and `LLM` environment variables do not change the format. Use
`--json` explicitly in scripts; `decompose --json ps` and
`decompose ps --json` are equivalent. `--table` explicitly selects text and
cannot be combined with `--json`.

Normal `ps` text output starts with the process table. A summary appears for
other conditions, such as `daemon not running` or
`daemon running; no processes started`. Do not parse these human-readable
messages to decide whether a command succeeded: use its exit status and JSON.

## Results

Finite queries and lifecycle commands produce one JSON document on stdout
when they succeed. A failed command emits a terminal diagnostic on stderr
instead of a success document. Warnings may appear on stderr even when the
command succeeds.

Results, diagnostics, and stream records use `schema_version: "1.0"`.
Consumers should ignore unfamiliar fields and event types.

| Command | Result |
|---------|--------|
| `ps` | `daemon` and `processes`, including each process's state and readiness. |
| `ls` | `environments`, each with `instance`, `daemon`, and an optional diagnostic when it cannot be reached. |
| `start`, `stop`, `restart`, `kill`, `down` | `operation`, `outcome`, `daemon`, and per-instance `services` outcomes. |
| Detached `up` | An operation result plus `daemon_action`, `changes`, and `readiness`. |

For example, `decompose --session demo ps --json` when no daemon exists:

```json
{"schema_version":"1.0","daemon":{"state":"not_running","pid":null,"instance":"6242c7883a5927e9"},"processes":[]}
```

Daemon absence is a successful empty query. An IPC connection failure is an
error, not proof that the daemon stopped. Process states distinguish
`failed_to_start` (the main process did not start) from `failed` (a nonzero
exit). An existing daemon can also have no running processes.

A successful `decompose --session demo up -d --wait --json` for a project
with one `web` service looks like this (the PID varies):

```json
{
  "schema_version": "1.0",
  "operation": "up",
  "daemon": {"state": "running", "pid": 12345, "instance": "6242c7883a5927e9"},
  "outcome": "completed",
  "services": [{"name": "web", "outcome": "ready"}],
  "changes": {
    "added": ["web"], "changed": [], "removed": [],
    "orphans": [], "renamed": [], "scaled": []
  },
  "daemon_action": "started",
  "readiness": "satisfied"
}
```

The overall `outcome` is `accepted`, `completed`, or `unchanged`. An accepted
start or restart means the daemon accepted the request; it does not guarantee
that the service is ready or will stay running. Similarly, a completed `kill`
confirms signal delivery, not process death.

`up -d --wait` waits for initialization hooks and the selected services'
started/healthy criteria before returning. Dependencies are included unless
`--no-deps` is used for the wait selection. On an existing daemon, that flag
does not prevent the start request from launching dependencies; see
[service selection](managing-projects.md#service-selection-and-disabled-services).
Readiness is a point-in-time check, not a continuing
health guarantee. With no eligible services, the wait succeeds immediately.
The flag requires `-d` and conflicts with `--no-start`.

Without `--wait`, detached `up` reports `readiness: "not_requested"`.
If waiting fails or times out, the environment remains running; diagnostics
include the affected process snapshots. Inspect it with `ps` and `logs`.
The wait deadline defaults to five minutes and can be set with
`DECOMPOSE_DAEMON_READY_TIMEOUT_MS`.

## Log and session streams

`logs` (including without `-f`), `attach`, and attached `up` emit JSON Lines:
one JSON object per line, rather than one array or result document. Empty
log selections emit nothing. Streams include application output and lifecycle
events; attached commands also include session events such as `attached` and
`detached`.

Representative application log records:

```json
{"schema_version":"1.0","type":"log","timestamp":"2026-09-29T12:00:00Z","service":"web","replica":1,"process":"web","stream":"stdout","message":"Listening on port 8080","partial":false}
{"schema_version":"1.0","type":"log","timestamp":"2026-09-29T12:00:01Z","service":"web","replica":1,"process":"web","stream":"stderr","message":"Request failed","partial":false}
```

Both records are emitted on **decompose's stdout**. The `stream` field tells
you which application stream produced the message; application stderr is log
data, not a CLI diagnostic. Hook output also carries `hook_phase`,
`hook_name`, and `hook_stage` when available. `partial: true` marks a chunk
split at the record size limit. Legacy log lines can have a null timestamp
when the original time is unknown.

Filter by event type before reading log-specific fields:

```sh
decompose logs --json | jq 'select(.type == "log" and .stream == "stderr")'
decompose logs -f --json | jq --unbuffered -r 'select(.type == "log") | .message'
```

## Diagnostics and exit status

In JSON mode, stderr contains one JSON diagnostic per line. Each has
`severity`, `code`, and `summary`; it can also contain `causes`, `usage`,
`hint`, `context`, source locations, and structured `details`. Use `code` for
programmatic decisions rather than matching wording in `summary`.
Do not merge stderr into stdout before parsing results or streams.

| Exit status | Meaning |
|-------------|---------|
| `0` | Successful operation or query, including empty query results. |
| `1` | Runtime failure. |
| `2` | Invalid CLI arguments. |
| Child status | `run` and `exec` propagate the one-off command's exit status. |

A read-only command whose output consumer closes early exits quietly, so
commands such as `decompose logs --json | head -n 1` need no special handling.

## A startup script

This Bash example requires `jq`. It saves the result separately from
diagnostics, preserves the startup exit status, and only continues when
readiness has been satisfied. Run it from your project's directory.

```bash
#!/usr/bin/env bash
set -euo pipefail

result_dir=$(mktemp -d)
trap 'rm -rf "$result_dir"' EXIT

if decompose up -d --wait --json \
    >"$result_dir/up.json" 2>"$result_dir/diagnostics.jsonl"; then
    # Successful commands can still report warnings.
    cat "$result_dir/diagnostics.jsonl" >&2
else
    status=$?
    cat "$result_dir/diagnostics.jsonl" >&2
    # Waiting does not stop detached services on failure.
    decompose ps >&2 || true
    decompose logs --no-pager -n 50 >&2 || true
    exit "$status"
fi

jq -e '.schema_version == "1.0" and .readiness == "satisfied"' \
    "$result_dir/up.json" >/dev/null
printf '%s\n' 'Services are ready.'
# Run your integration tests here.
```

This leaves the environment available for inspection. In a disposable CI
project, arrange for `decompose down` in the job's cleanup step, including on
failure. Keep using the same config files and session for every command.

Other useful queries:

```sh
decompose ps --json | jq '.processes[] | select(.state == "running")'
decompose ls --json | jq -r '.environments[].instance'
```

## Native output exceptions

- `config` prints resolved YAML in text mode and its configuration and
  provenance object in JSON mode; it does not use the operation result above.
- Successful help, version, and shell completion output stays native text.
- `run` and `exec` pass the child's stdout and stderr through unchanged.
  Output flags after the service/command boundary belong to the child.
- `--json` cannot be used with `tui` or `up --tui`; the command is rejected
  before services start or a terminal opens.
