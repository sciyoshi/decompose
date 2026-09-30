# CLI output

Text is the default, including when stdout is redirected. `CI` and `LLM` do
not select an output format. Use the global `--json` flag for scripts; both
`decompose --json ps` and `decompose ps --json` work. `--table` explicitly
selects text and conflicts with `--json`.

This is a breaking output change. Scripts relying on implicit piped JSON,
`ps.running`, top-level `up.pid`, acknowledgment `status`/`message` fields,
or `ls.environments[].name` must migrate before the next breaking release.
No package version or release tag is changed by this implementation.

## Results and streams

Finite commands emit one JSON result on stdout after their completion
condition is satisfied. On failure there is no success document; stderr
contains one terminal diagnostic. Warnings are separate diagnostic records.
Every result, diagnostic, and stream record carries `schema_version: "1.0"`.
Consumers should ignore unfamiliar fields and event types.

- `ps` returns `daemon: {state, pid, instance}` and `processes`. Daemon absence
  is `not_running`, with a null PID and an empty array. Failed IPC is an error,
  not evidence that the daemon stopped. Process states distinguish
  `failed_to_start` from a nonzero exit (`failed`). Text output starts directly
  with the process table when processes are running; summaries appear for
  other states, such as daemon absence or no processes started.
- `ls` returns `environments`, each with `instance`, `daemon`, `project_dir`,
  `config_files`, `process_count`, and an optional
  diagnostic for an unreachable environment.
- Operations return `operation`, `outcome`, `daemon`, and per-instance
  `services: [{name, outcome}]`. Overall outcomes are `accepted`, `completed`,
  and `unchanged`. Accepted starts and restarts are requests, not readiness
  guarantees. A completed `kill` confirms signal delivery, not process death.
- `up` also reports `daemon_action`, `changes`, and `readiness`.
  Changes contain arrays for `added`, `changed`, `removed`, `orphans`,
  `renamed`, and `scaled`. `up -d --wait` emits its result only after readiness;
  failure details describe the environment left running. `--wait` conflicts
  with `--no-start`.
  In text mode, `up -d --wait` also prints lifecycle progress on stdout while
  waiting: process starts/exits, pre/post-start hook starts and outcomes, and
  changes in process state, initialization stage, and readiness. Only selected
  services (including dependencies unless `--no-deps`) are shown; previous
  lifecycle events and application/hook command output are omitted. Use
  `logs -f` to see command output. JSON mode retains one final result, with
  failures and warnings on stderr.
  On a TTY, progress updates in place: each service/replica has a spinner,
  ready checkmark, or failure marker, with its current state and hook progress.
  State columns align, and markers and states share the `ps` color palette
  (respecting `NO_COLOR`). Ready rows show only `ready`; completed hook counts
  disappear, including while a service is still waiting for readiness.
  The overall bar counts ready services, not elapsed time. The display uses
  only its own lines, leaves the final state visible, and does not clear the
  screen or enter an alternate screen. Long rows are truncated; short terminals
  prioritize failures and pending services. A resize starts a fresh block to
  preserve earlier terminal output. Redirected output and `TERM=dumb` use
  line-by-line lifecycle messages.
- `logs`, `logs -f`, `attach`, and attached `up` emit JSON Lines. Application
  stderr remains stdout data with `stream: "stderr"`. Log records retain
  service, process, replica, timestamp, message, partial-record, and hook
  attribution. Lifecycle records have their own `type`. Legacy daemon log
  lines have a null timestamp when their original occurrence time is unknown.
  Empty log selections emit no records.

```sh
decompose ps --json | jq '.processes[] | select(.state == "running")'
decompose ls --json | jq -r '.environments[].instance'
decompose up -d --wait --json | jq '.readiness'
decompose logs --json | jq 'select(.type == "log" and .stream == "stderr")'
```

## Diagnostics

Diagnostics contain `severity`, `code`, and `summary`, with optional `causes`,
`usage`, `hint`, and typed `details`. Source causes preserve OS error numbers
when available. Readiness failures include the daemon and affected process
snapshots; independent cleanup failures remain separate entries. Legacy daemon
errors use `remote_error`, preserving the remote message without guessing its
meaning. Mutating requests check daemon capabilities before sending work;
restart an older daemon to use the new protocol.

Exit status is 0 for successful operations and empty queries, 1 for runtime
failures, and 2 for invalid arguments. One-off child commands retain their
exit status. A closed consumer of read-only output terminates quietly.

## Exceptions

`config` retains resolved YAML in text mode and its established configuration
and provenance object in JSON mode. Successful help, version, and completion
output stays native text. `run` and `exec` pass child streams through unchanged;
output flags after the service/command boundary belong to the child.
`--json` is incompatible with `tui` and `up --tui` and is rejected before
starting services or opening a terminal.

The library's `run_cli()` still returns errors to its caller. Applications
that want the binary's explicit rendering and exit-code behavior can call
`run_cli_from(args, stdout, stderr)` with their own writers. Nonzero one-off
child statuses are returned to `run_cli()` callers as `ChildExitStatus`, rather
than terminating the embedding process.
