# Proposal: shared environment defaults with nested interpolation

Status: proposed. Date: 2026-09-28. Global `environment` already exists;
the interpolation changes specified here are not implemented yet.

## Problem

Reusable service fragments need a configurable data directory. Fellow's
Python CLI supplies `STACK_DATA_DIR=~/.local/share/fellow` when the variable
is unset. An explicitly empty value, or running decompose directly without
the variable, should select `$PWD/.data`.

Today, every command must repeat shell expansion:

```yaml
command: clamd --config-file="$${STACK_DATA_DIR:-$${PWD}/.data}/clamd.conf"
```

The same expression appears in initialization commands, guards, database
updates, and health probes. Assigning a shell variable in one hook does not
help: hooks and probes have separate environments and processes.

Decompose already has the right configuration mechanism: a top-level
`environment` block, inherited by services and their hooks. Two limitations
prevent expressing this default there:

- `${VAR:-default}` currently falls back only when the variable is unset,
  retaining an empty value instead of using the default.
- The regex interpolator stops at the first closing brace. Nested defaults
  are not parsed correctly, even when the outer variable has a value.

For example, the installed 0.3.3 build resolves
`${STACK_DATA_DIR:-${PWD}/.data}` as follows:

| Incoming `STACK_DATA_DIR` | Current result |
|---|---|
| Unset | `${PWD/.data}` |
| Empty | `/.data}` |
| `/tmp/custom` | `/tmp/custom/.data}` |

These results were reproduced using `decompose config --json` with a
temporary config and dotenv loading disabled.

## Desired configuration

Define the default once in the root configuration:

```yaml
environment:
  STACK_DATA_DIR: "${STACK_DATA_DIR:-${PWD}/.data}"

include:
  - decompose/clamd.yaml
```

Included fragments can then use the inherited value everywhere:

```yaml
processes:
  clamd:
    command: clamd --config-file="$${STACK_DATA_DIR}/clamd.conf" --foreground
    readiness_probe:
      exec:
        command: clamdscan --config-file="$${STACK_DATA_DIR}/clamd.conf" --version
```

The `$${STACK_DATA_DIR}` references deliberately defer substitution to the
child shell. Quoting the expansion preserves spaces and keeps characters
inside an environment value from becoming shell syntax. No per-command
fallback or local `data_dir` assignment is necessary.

The root owns this policy; service fragments consume the resolved variable.
A standalone fragment may be supplied a value by its caller. No new YAML
key, setup process, environment-producing hook, or shell evaluation during
configuration loading is required.

Projects wanting a location independent of the invocation directory can use
`${STACK_DATA_DIR:-${DECOMPOSE_PROJECT_DIR}/.data}` instead. Preserve the
distinction between the existing `PWD` variable and the reserved project
anchor; do not silently change their meanings.

## Prior art

Process Compose supports global and per-process environment declarations
and uses `envsubst` for variable expansion. Its global declaration is the
appropriate model for native processes sharing project defaults.
[Process Compose configuration](https://f1bonacc1.github.io/process-compose/configuration/#environment-variables).

Docker Compose uses `.env` or `--env-file` values for shared configuration
interpolation and service-level `environment` for container environments.
Its interpolation supports nested expressions and distinguishes an unset
variable from an empty one. Adopt its default-expression semantics without
claiming full Compose interpolation compatibility in this change.
[Docker Compose interpolation](https://docs.docker.com/reference/compose-file/interpolation/),
[interpolation environment](https://docs.docker.com/compose/how-tos/environment-variables/variable-interpolation/).

## Required interpolation semantics

Support these forms consistently in every field that already interpolates:

| Expression | Unset | Empty | Nonempty value `v` |
|---|---|---|---|
| `$VAR` or `${VAR}` | Empty string | Empty string | `v` |
| `${VAR:-fallback}` | Expand fallback | Expand fallback | `v` |
| `${VAR-fallback}` | Expand fallback | Empty string | `v` |
| `$$` | Literal `$` | Literal `$` | Literal `$` |

An empty string is exactly zero characters. Whitespace, `0`, and `false`
are nonempty values. `${VAR:-}` and `${VAR-}` are valid empty defaults.

Parse balanced nested parameter expressions. Examples:

```text
${STACK_DATA_DIR:-${PWD}/.data}
${STACK_DATA_DIR:-${DECOMPOSE_PROJECT_DIR}/.data}
${A:-${B:-fallback}}
${A-${B-default}}
```

Evaluate only the selected fallback branch. With `A=value`,
`${A:-${B}/suffix}` produces exactly `value`, including when `B` is unset.
Preserve literal prefixes, suffixes, adjacent substitutions, Unicode, and
paths containing spaces.

Recursion applies to expressions written in the configuration, not to the
contents of substituted values. If an incoming value contains `${OTHER}`,
return those characters literally. Do not repeatedly expand the output
until stable, execute `$(...)`, evaluate backticks, or invoke a shell.

An escaped `$` is not reconsidered during that field's interpolation pass.
For example, `$${VAR:-$${PWD}/.data}` must remain the literal shell expression
`${VAR:-${PWD}/.data}`. Preserve existing behavior for lone dollars and
unclosed expressions outside the newly supported grammar.

Bound nested expression depth at 32 levels and return a configuration error
when exceeded. Identify the file and field without dumping environment
values. This bounds parser work without changing ordinary configurations.

## Environment resolution and includes

Preserve existing layering and merge behavior; this proposal changes
expression parsing, not the precedence of configuration sources.

For the global self-defaulting declaration, resolve `STACK_DATA_DIR` against
the incoming interpolation environment before inserting the declaration's
result. In particular, the right-hand side must not see itself as the raw
string `${STACK_DATA_DIR:-...}`. The shell environment continues to override
root dotenv values for interpolation, including an explicitly empty value.

The resolved global value is inherited by service commands, both hook
phases, guards, hook wait probes, health probes, shutdown commands, and
one-off commands according to their existing environment rules. Explicit
service and hook overrides keep their current precedence. A hook override
must not leak into other hooks or the service.

Global environment entries currently resolve in key order. Preserve that
behavior for this change and document it; arbitrary forward references and
dependency-graph evaluation are outside this proposal. The motivating
self-default references an incoming variable, not another global key.

Preserve the existing distinctions between interpolation sources and child
environment sources. In particular, do not make per-process `env_file`
values available for configuration interpolation as an incidental change.

Includes continue to merge raw definitions before ordinary field
interpolation. A root environment declaration can therefore supply defaults
to imported services. Preserve field origins: nested references to
`DECOMPOSE_FILE_DIR` use the file supplying the containing field or
environment declaration; `DECOMPOSE_PROJECT_DIR` uses the first root config.

Use the same expression parser for include paths, which retain their
existing earlier discovery phase and narrower variable context. Imported
environment values must not retroactively affect include discovery.

Keep `disable_env_expansion` behavior unchanged, including the existing
exception that include discovery still interpolates paths. Keep deferred
hook interpolation to one pass; do not expand an escaped dollar once while
loading the service and again while resolving the hook.

## Observable behavior

For `PWD=/checkout/stack`, the motivating global declaration must yield:

| Invocation input | Resolved `STACK_DATA_DIR` |
|---|---|
| CLI wrapper supplies `/home/alice/.local/share/fellow` | `/home/alice/.local/share/fellow` |
| Caller supplies `/mnt/dev data` | `/mnt/dev data` |
| Caller supplies an empty string | `/checkout/stack/.data` |
| Direct decompose with the variable absent | `/checkout/stack/.data` |

The Python wrapper remains responsible for its home-directory default;
decompose must not introduce a Fellow-specific default or expand `~` as part
of parameter interpolation.

`decompose config --json` must expose the resolved global value. Escaped
command references remain escaped until the child shell runs. The daemon,
preflight, `run`, `exec`, and reload paths must consume the same resolved
configuration. Changes to an effective environment value must participate
in normal service hashing and recreation; no separate environment cache is
introduced.

## Implementation outline

1. Replace the flat regex expansion in `src/config.rs` with a small balanced
   parser for the supported parameter grammar. Preserve the unset/empty
   distinction through lookup and evaluate selected fallback expressions.
2. Route all existing interpolation call sites through it, including the
   origin-aware loader in `src/config/compose.rs`, include discovery, and
   deferred hook resolution. Ensure errors carry source/field context.
3. Keep environment composition separate from parsing. Verify global
   self-defaulting against incoming values without changing key ordering,
   per-process resolution, or source precedence.
4. Replace `interpolate_nested_default_is_not_recursive` with tests for the
   new contract, and add unset-versus-empty coverage. That test currently
   records the behavior this proposal intentionally replaces.
5. Update the configuration and migration guides, add a shared-data-directory
   example, and document the compatibility change in release notes.

## Validation and acceptance criteria

| Area | Required coverage |
|---|---|
| Operators | Unset, empty, and nonempty inputs for plain lookup, `:-`, and `-`; empty defaults; whitespace values. |
| Nesting | Nested defaults, suffixes, adjacent expressions, selected/unselected branches, reserved anchors, depth limit. |
| Literal data | `$$`, escaped nested shell expressions, substituted values containing dollar syntax, Unicode, spaces, no command execution. |
| Global environment | Self-default from shell and dotenv; empty shell value overrides nonempty dotenv before fallback; inherited value reaches children. |
| Overrides | Global, service, and hook precedence remains unchanged; hook overrides do not leak; existing key-order behavior remains covered. |
| Includes | Root defaults apply to included services; declaration origins survive overlays; project/file anchors resolve correctly inside defaults. |
| Hooks and probes | Guards, commands, wait probes, readiness/liveness, and shutdown receive the same directory; escaping is consumed once. |
| CLI and daemon | `config`, `up`, reload, `run`, and `exec` agree on values; changes recreate affected services; disabled expansion stays unchanged. |

Use isolated temporary projects and stub commands; no ClamAV installation,
network downloads, real user data, or Fellow checkout is required. Exercise
all four invocation inputs above. Have the service and its hooks record
the directory they receive, and assert that each uses the same value with
no repeated fallback expressions in commands. Clean up every test daemon.

Before implementation is complete, run the repository's required build,
test, formatting, clippy, and Rust documentation checks. This proposal alone
does not alter runtime behavior.

## Compatibility and scope

Correcting `:-` for empty values and nested expressions changes existing
behavior. Users who intentionally want an empty value preserved should use
`${VAR-default}`. Users who want shell-time expansion should retain `$$`.
Explain these migrations explicitly rather than hiding the change behind
an interpolation-version flag.

Defer required-value and alternative-value operators (`:?`, `?`, `:+`, `+`),
pattern substitution, environment-command execution, a new `defaults` or
`vars` block, arbitrary environment dependency graphs, and broader dotenv
parser changes. Those can be separate proposals. The acceptance criterion
here is a single shared environment default that every service command and
hook can consume reliably.
