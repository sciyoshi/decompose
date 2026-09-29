# Managing a running project

Decompose runs one daemon per environment. The daemon owns the services;
commands in other terminals talk to it over a local socket.

## Ownership and attaching

`decompose up` starts a daemon and streams logs. If it creates the daemon,
that foreground command owns the environment: Ctrl-C shuts everything down.
If the daemon already exists, foreground `up` reloads configuration and
attaches as a viewer; Ctrl-C only detaches that viewer.

Use `decompose up -d` to keep services running independently of your terminal.
Later, `decompose attach` or `decompose logs -f` streams their logs without
reloading configuration. Ctrl-C leaves the daemon running. The TUI also
leaves it running on `q` or Ctrl-C; uppercase `Q` shuts the environment down.

## Targeting the same environment

By default, identity comes from the first configuration file's directory and
the set of root configuration files. File contents and included fragments do
not change identity. Reordering the same root files does not change their
identity (when the first file's directory stays the same), even though merge
order matters. Adding a root file normally targets a different environment.

From another terminal, use the same project directory and file arguments:

```sh
cd /path/to/project
decompose -f decompose.yaml -f local.yaml up -d
# In a second terminal:
decompose -f /path/to/project/decompose.yaml -f /path/to/project/local.yaml ps
```

An explicit `--session NAME` makes identity depend only on that name.
`DECOMPOSE_SESSION` supplies the same setting through the environment. Use
distinct names for environments you want to keep separate:

```sh
decompose -f /path/to/project/decompose.yaml --session demo up -d
decompose -f /path/to/project/decompose.yaml --session demo logs -f
```

You can also use a full instance ID printed by `decompose ls`, for example
`decompose --session 0123456789abcdef ps`. Session values consisting of exactly
16 lowercase hex digits are interpreted as instance IDs, not names.
Inspection and control commands such as `ps`, `logs`, `attach`, and `down`
can target a session from any directory without `--file`. Commands that load
configuration, such as `up`, still need the correct configuration path.
Keep the same runtime directory settings across terminals so commands find
the same socket.

## Choosing a command

| Goal | Command | Effect |
|------|---------|--------|
| Apply configuration edits | `decompose up -d` | Reload files, reconcile services, and start eligible stopped services. |
| Resume stopped services | `decompose start web` | Use the daemon's stored configuration; also start dependencies. |
| Rerun a service or retry hooks | `decompose restart web` | Stop and relaunch using the stored configuration. |
| Pause a service | `decompose stop web` | Stop it while keeping the daemon and its registered services. |
| Stop the environment | `decompose down` | Stop services and exit the daemon. |

`start` and `restart` do not reread YAML or env files. `up` keeps unchanged
running services in place; it is not an unconditional restart.

### Service selection and disabled services

On a fresh daemon, `up -d web` starts the selected services and their
dependencies, leaving other services registered but not started. `--no-deps`
omits the dependency expansion on this initial launch; dependency conditions
still apply, so the selected service can remain pending.

**Against an existing daemon, every `up` reloads the whole project**, even
`up -d web`. Added or changed services elsewhere in the project may start or
be recreated. The subsequent start request targets the named services and
expands their dependencies; currently `--no-deps` does not suppress that
expansion on an existing daemon. Use `start web` when you want to resume it
without applying project-wide file edits.

Services with `disabled: true` are not automatically started. On a fresh
daemon, naming one in `up` does not override that flag. Use `start NAME` to
explicitly enable and start it; this also enables disabled dependencies.
On an existing daemon, `up NAME` issues that explicit start after reloading.
A later reload restores the disabled setting from YAML. Blanket `start` and
`up` skip disabled services.

## What changes take effect when?

| Change | How to apply it |
|--------|----------------|
| Service YAML, including commands, probes, hooks, and environment | Run `up`; services whose resolved configuration changes are recreated. |
| `.env`, the original `-e` files, or a service's `env_file` contents | Run `up`; changes to the resolved service environment trigger recreation. |
| Variables exported in your terminal | Run `down`, then `up` from the updated shell. The daemon retains its original inherited shell environment. |
| Contents of a script or executable referenced by a command | Run `restart SERVICE` if the application does not reload it itself. Script contents are not part of the configuration hash. |
| Only `replicas` | Run `up`; scale up by adding replicas or scale down by stopping surplus replicas, keeping surviving instances. |
| Remove a service from YAML | Run `up --remove-orphans` to stop and unregister it; otherwise it remains in the daemon with a warning. |

A reload uses the daemon's **original root file order, `-e` file list, and
`--disable-dotenv` setting**. Passing different launch settings to a later
`up` does not replace those settings in the existing daemon. Restart the
environment with `down` and `up` to change them. This matters especially when
using a session name, which can target the same daemon from different paths.

Shell variables inherited by the daemon also take precedence during
interpolation, so a stale shell value can mask an edited dotenv value. See
[Environment and interpolation](environment.md) for
precedence and interpolation syntax. `config`, `run`, and `exec` resolve files
in the current CLI process and can therefore see different shell values.

### Controlling reconciliation

- `up --force-recreate` recreates existing services even when their resolved
  configuration is unchanged. This applies across the project.
- `up --no-recreate` preserves existing service definitions when their hashes
  differ. It still adds new services and applies replica-only scaling changes;
  it also applies a `disabled` toggle when no other hashed configuration
  changes. It conflicts with
  `--force-recreate`.
- `up --no-start` registers added/recreated services and new replicas without
  launching them. It can still stop services being replaced or removed; it
  is not a preview. A disabled-to-enabled toggle on an existing service can
  still queue that service to start. Use `config` to inspect resolved YAML
  without changing the daemon.

For readiness checks, see [startup initialization and waiting](commands.md#startup-initialization-and-waiting)
and the [automation example](output.md).
