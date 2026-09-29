# Includes and packaged fragments

A top-level `include` list imports processes and global environment:

```yaml
include:
  - ${FLOX_ENV}/share/decompose/nats.yaml
  - path: ${FLOX_ENV}/share/decompose/temporal.yaml
    processes: [temporal]

processes:
  nats:
    environment:
      NATS_PORT: '4223'
  api:
    command: ./api
    depends_on:
      nats: {condition: process_started}
```

Paths are interpolated and resolved relative to the file containing the
include. Missing files are errors. Includes may nest, with cycle detection
using canonical paths and a maximum depth of 32 edges.

Files merge in include-list order, followed by the including file's local
definitions. Two imports defining the same process are an error unless the
immediate including file also defines that process. A partial local definition
is sufficient: the imports merge in order, then the local fields override them.
An ancestor's definition cannot resolve a conflict inside a nested include.

`processes` selects names from the composed included file and brings their
transitive dependencies. Omit it to import all processes; use `processes: []`
to import only global environment. Unknown selected names are errors. Global
environment is always imported, with later values winning. Dependencies may
also refer to processes supplied elsewhere in the final project.

Only processes and environment are imported. `exit_mode` and
`disable_env_expansion` are controlled by root config files. With multiple
`--file` arguments, each file's include tree is composed before applying the
root-file overlays in argument order.

The first root config determines the project directory and automatic `.env`.
Included files never load their own `.env`, and their paths are not added to
the instance identity. `up` on an existing daemon rereads included files;
changes to effective process configuration participate in normal reloads.

## Path anchors

Two reserved interpolation variables are supplied by decompose:

| Variable | Value | Typical use |
|---|---|---|
| `DECOMPOSE_PROJECT_DIR` | Canonical directory of the first root config | Writable data directories |
| `DECOMPOSE_FILE_DIR` | Canonical directory of the file supplying the value | Assets next to a fragment |

Field origins survive partial overrides. For example, an inherited command
uses the fragment's `DECOMPOSE_FILE_DIR`, while a locally overridden environment
value uses the local file's directory. These are interpolation variables, not
automatically exported child environment variables. Environment declarations
cannot override their meaning during interpolation.

Relative `working_dir` and `env_file` paths use the **project directory**, even
inside fragments. An `env_file` can resolve beneath either the project directory
or the directory of the file that supplied that list. Canonical paths are checked,
so symlinks cannot escape those directories. For a packaged asset, use:

```yaml
env_file: ['${DECOMPOSE_FILE_DIR}/defaults.env']
working_dir: ${DECOMPOSE_PROJECT_DIR}
```

Include-path interpolation uses root `.env` / explicit `--env-file` values,
shell environment, the declaring file's own global environment, and the anchors.
Imported environment does not affect include discovery. Include-path expansion
still runs when `disable_env_expansion: true`; that setting suppresses expansion
of the composed config's content.

This syntax follows Compose's `include` naming, but local conflict overrides,
process selection, root-relative service paths, and root-only dotenv loading
are deliberate differences from the Compose specification.

## Provenance

`decompose config --json` adds a `provenance.processes` map. Each entry contains
`command_source` (the canonical file that supplied the command) and `files`
(contributing files in first-contribution order). Internal field origins also
track environment keys individually. Provenance metadata alone does not change
process hashes or trigger a restart. Table/YAML output contains only the
resolved configuration.

## Building fragments with Nix

Use `decompose.lib.mkFragment` from the decompose flake input:

```nix
natsFragment = decompose.lib.mkFragment {
  inherit pkgs;
  name = "nats";
  src = ./nats.yaml;
  substitutions = { natsServer = pkgs.nats-server; };
};
```

The helper installs `share/decompose/nats.yaml`, replacing `@name@` placeholders
with the supplied values. It preserves decompose interpolation and dollar
escapes. The source fragment can contain:

```yaml
processes:
  nats:
    command: >-
      exec @natsServer@/bin/nats-server --jetstream
      --store_dir "${DECOMPOSE_PROJECT_DIR}/.data/nats"
      --port "$${NATS_PORT:-4222}"
```

Combine fragments into one installable package:

```nix
stack-services = pkgs.symlinkJoin {
  name = "stack-services";
  paths = [ natsFragment temporalFragment ];
};
```

Absolute store paths let services run without their binaries on `PATH` or an
active Flox environment. Nix retains binaries referenced in the output as runtime
dependencies; merely adding a package to build inputs does not retain it. Reference
any required CLI tools in the shipped configuration or assets too. The consumer
can then pin one fragment package, together with its binary closure, in its Flox
manifest lock. The stack repository can include these same installed fragments.

