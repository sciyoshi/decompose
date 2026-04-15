# Getting Started

## Installation

### From crates.io

```bash
cargo install decompose
```

### Prebuilt binaries

Download a tarball for your platform from the [latest release](https://github.com/sciyoshi/decompose/releases/latest), extract it, and put `decompose` on your `$PATH`. Builds are published for:

- `x86_64-unknown-linux-gnu`
- `aarch64-unknown-linux-gnu`
- `x86_64-apple-darwin`
- `aarch64-apple-darwin`

## Your first project

Create a `decompose.yaml` in your project directory:

```yaml
processes:
  web:
    command: "python -m http.server 8000"
  worker:
    command: "echo 'worker started' && sleep infinity"
    depends_on:
      web:
        condition: process_started
```

Then start it:

```bash
decompose up -d
```

Check the status:

```bash
decompose ps
```

View logs:

```bash
decompose logs -f
```

Stop everything:

```bash
decompose down
```

Full documentation coming soon.
