# Environment and interpolation

Decompose handles variables at two different times: it substitutes values in
YAML while loading configuration, then supplies an environment to each child
process when launching it. A value available during substitution is not
necessarily the value the child receives.

## Interpolation versus runtime expansion

`${VAR}` and `$VAR` in YAML are expanded by decompose. Escape the dollar sign
as `$$` to leave expansion to the service's shell:

```yaml
processes:
  example:
    command: 'printf "configured=%s runtime=%s\n" "${MESSAGE}" "$${MESSAGE}"'
    env_file: [example.env]
```

With `MESSAGE=from-file` in `example.env`, launch from a shell with
`MESSAGE=from-shell`:

```sh
MESSAGE=from-shell decompose up
# example prints: configured=from-shell runtime=from-file
```

The service's `env_file` participates in its runtime environment, but not in
ordinary YAML interpolation. Decompose substitutes `${MESSAGE}` from the
shell and turns `$${MESSAGE}` into `${MESSAGE}`. The service's shell expands
that remaining reference using the child environment. YAML single quotes
do not disable decompose interpolation.

### Interpolation precedence

For ordinary service fields, later sources override earlier ones:

1. The root `.env` file, then global `-e/--env-file` files in argument order.
2. The shell environment of the process loading configuration.
3. The global YAML `environment` block.
4. The service's YAML `environment` block.
5. Reserved `${DECOMPOSE_PROJECT_DIR}` and `${DECOMPOSE_FILE_DIR}` anchors.

The anchors identify the first root config's directory and the directory
that supplied the individual field, respectively. They are interpolation
values, not automatically injected child variables. See
[includes](configuration.md#includes-and-packaged-fragments) for their use with fragments.

Global environment values expand in alphabetical key order, so a later key
can refer to an earlier resolved key. Service environment values and other
service fields use a snapshot containing the service's unexpanded environment
values. Substitution is not recursive: putting `${OTHER}` inside a variable's
value does not make decompose expand it again. Prefer explicit values or
runtime shell expansion over chains of service environment references.

| Syntax | Behavior |
|--------|----------|
| `${VAR}` or `$VAR` | Substitute the value; use an empty string if unset. |
| `${VAR:-default}` | Use `default` only when `VAR` is **unset**. An explicitly empty value stays empty, unlike shell `:-` expansion. |
| `$$` | Produce one literal dollar sign, without expanding the following reference in this pass. |

Defaults are literal text, not recursive expressions; avoid nested defaults
such as `${A:-${B:-fallback}}`. Other shell parameter operators are not
supported by decompose. Use escaped runtime expansion when you need them.

Interpolation applies to commands, descriptions, working directories,
`env_file` paths, `ready_log_line`, shutdown commands, probe exec commands,
HTTP probe host/scheme/path, and YAML environment values. Set the top-level
`disable_env_expansion: true` to disable content interpolation; include paths
still expand. Startup hooks expand later, after service env files are loaded;
see [startup hooks](configuration.md#startup-hooks) for their rules.

## The child environment

For supervised services, the inherited daemon environment is the base. The
following configured layers override it, from lowest to highest priority:

| Layer | Source |
|-------|--------|
| 1 | Root `.env`, automatically loaded from the first config file's directory. |
| 2 | Global `-e/--env-file` files, with later files winning. |
| 3 | Global YAML `environment`. |
| 4 | Service `env_file` entries, with later files winning. |
| 5 | Service YAML `environment`. |

All replicas of a service receive the same resolved environment.

Global `--disable-dotenv` skips only the automatic root `.env`; explicit
`-e` files still load. Service `is_dotenv_disabled: true` removes keys supplied
only by the root `.env`/global `-e` layer from that service's configured
environment. It does not prevent their use in interpolation, remove explicit
YAML or service `env_file` overrides, or clear the daemon's inherited variables.

`run` and `exec` instead start with a **cleared environment**, then apply the
same configured layers. They resolve those layers locally from disk using
the current CLI's shell for interpolation. Unrelated exported shell variables
are not automatically passed to the one-off child. Use command-level `--env` to
override a value or copy one from the calling shell:

```sh
# Global -e selects a file; run's --env selects a child variable.
decompose -e local.env run --env DEBUG=true --env PATH example /usr/bin/env
```

A bare command-level `--env KEY` copies `KEY` from the current shell (or uses an
empty string if absent). These overrides affect the one-off child's environment,
not configuration interpolation. `exec` checks that the service is running,
but launches a new local process; it does not retrieve the running service's
environment. See [one-off commands](commands.md#ad-hoc-commands).

## File formats and paths

YAML environment blocks accept either a map or a list of `KEY=VALUE` strings:

```yaml
environment:
  PORT: "3000"
  DEBUG: "true"
```

The equivalent list is:

```yaml
environment:
  - PORT=3000
  - DEBUG=true
```

Env files use simple `KEY=VALUE` lines:

```dotenv
# Local settings
PORT=3000
MESSAGE="hello world"
```

Blank lines and full-line comments are ignored. An optional `export ` prefix
and matching outer quotes are removed. Values are not shell-evaluated or
interpolated by the env file parser; `URL=${HOST}` remains that literal value.
Use comments on separate lines, since inline comments are part of the value.
Malformed lines produce a warning and are skipped.

Relative global `-e` paths and service `env_file` paths are based on the first
config file's directory. Included fragments do not load their own `.env`.
A fragment can use `${DECOMPOSE_FILE_DIR}` to locate its env file. Service
env files must resolve within the project or their defining file's directory;
symlink targets are checked too.

## Refreshing variables in a running project

The daemon inherits its shell environment when it starts. Later commands do
not send their shell environment to it:

```sh
MESSAGE=old decompose up -d
MESSAGE=new decompose up -d  # the existing daemon still interpolates with old
```

To use the updated shell value, stop and recreate the environment:

```sh
decompose down
MESSAGE=new decompose up -d
```

`up` against an existing daemon rereads YAML and env file contents, using the
daemon's original root file order, global `-e` file list, and `--disable-dotenv`
setting. It recreates services whose resolved configuration changes, unless
`--no-recreate` prevents it. A stale inherited shell value can still override
an edited dotenv value during interpolation. Changing launch settings also
requires `down` followed by `up` with the new settings.

`start` and `restart` reuse stored definitions without rereading files.
`config`, `run`, and `exec` resolve files in the current CLI process, so they
can see different shell values from the existing daemon. `config` is a preview
of local resolution, not a dump of the daemon's effective environment. See
[managing a running project](managing-projects.md#what-changes-take-effect-when)
for other changes and how to apply them.
