# decompose

![decompose logo](assets/logo.svg)

**Run your stack at native speed.**  
`decompose` is a Rust process orchestrator for local development and agentic coding loops.

No image builds. No container cold starts. No bridge-network translation overhead.  
Just your real processes, fast, with a familiar compose-like interface.

## Installing

### From crates.io

```bash
cargo install decompose
```

Requires Rust 1.85 or later. If you don't have Rust installed, grab it from [rustup.rs](https://rustup.rs/).

### Prebuilt binaries

Download a tarball for your platform from the [latest release](https://github.com/sciyoshi/decompose/releases/latest), extract it, and put `decompose` on your `$PATH`. Builds are published for:

| Target | OS | Arch |
|---|---|---|
| `x86_64-unknown-linux-gnu` | Linux | x86_64 |
| `aarch64-unknown-linux-gnu` | Linux | ARM64 |
| `x86_64-apple-darwin` | macOS | Intel |
| `aarch64-apple-darwin` | macOS | Apple Silicon |

Quick install example (macOS Apple Silicon):

```bash
curl -sL https://github.com/sciyoshi/decompose/releases/latest/download/decompose-aarch64-apple-darwin.tar.gz \
  | tar xz -C /usr/local/bin
```

### With Nix

Run without installing:

```bash
nix run github:sciyoshi/decompose -- up
```

Or install into your profile:

```bash
nix profile install github:sciyoshi/decompose
```

You can also add it as a flake input in your own `flake.nix`:

```nix
inputs.decompose.url = "github:sciyoshi/decompose";
```

The flake also exposes a `devShell` for contributors — `nix develop` drops you into a shell with `cargo`, `rustc`, `rustfmt`, and `clippy` pinned.

### From source

```bash
git clone https://github.com/sciyoshi/decompose
cd decompose
cargo build --release
```

The binary will be at `target/release/decompose`. You can also use `cargo install --path .` to install it directly into your Cargo bin directory.

## Why this is better for day-to-day coding

- **Native performance**: run directly on host processes and filesystems.
- **Faster inner loops**: no Dockerfile rebuilds just to iterate on app code.
- **Lower complexity**: no container networking setup for every local workflow.
- **Agent-friendly**: predictable JSON/table output and deterministic control from other tabs.
- **Familiar UX**: `up`, `ps`, `down`, compose-style YAML, dependencies, replicas.

## Built for humans and agents

- `decompose up` starts and attaches.
- `Ctrl-C` detaches your terminal session while keeping the daemon alive.
- `decompose up -d` starts and returns immediately.
- `decompose ps` reports empty state instead of error when nothing is running.
- Use `decompose down` from any tab/agent to stop the environment.

## Reproducible with Nix

This repo ships a `flake.nix` so you can pair **Nix + decompose** and get most of Docker's local-dev benefits (isolated environments, consistent versions across machines) without container runtime overhead.

```bash
nix develop
cargo test
```

Nix pins the toolchain and dependencies; `decompose` orchestrates native processes on top of that reproducible environment.

## Commands

```bash
decompose up [-f FILE] [-d|--detach] [--json|--table]
decompose down [-f FILE] [--json|--table]
decompose ps [-f FILE] [--json|--table]
```

## Output modes

- `--json`: machine-readable
- `--table`: human-friendly
- default:
  - `table` when stdout is a TTY
  - `table` when `LLM=true` or `CI=true`
  - otherwise `json`

## Runtime model

- Per-environment daemon, isolated by working directory + config path hash.
- Local socket IPC via [`interprocess`](https://docs.rs/interprocess/latest/interprocess/local_socket/index.html).
- XDG-aware paths:
  - socket: `$XDG_RUNTIME_DIR/decompose/<instance>.sock` (fallbacks applied)
  - state: `$XDG_STATE_HOME/decompose/<instance>.pid` and `.log`

## Config discovery

If `-f/--file` is omitted, discovery order is:

1. `compose.yml`
2. `compose.yaml`
3. `decompose.yml`
4. `decompose.yaml`

## Example

```yaml
processes:
  hello:
    command: "echo hello"
  date:
    command: "date"
    depends_on:
      hello:
        condition: process_completed_successfully
```

```bash
decompose up
decompose ps
decompose down
```
