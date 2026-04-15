# AGENTS.md

Guidance for agents (human or automated) contributing to `decompose`.

## Project overview

`decompose` is a Rust process orchestrator for local development. See `README.md` for the pitch and `CLAUDE.md` for design notes.

## Repo structure

```
src/
  main.rs      Entry point; calls run_cli()
  lib.rs       CLI command handlers, log streaming
  cli.rs       Clap argument definitions
  config.rs    YAML parsing, merging, .env loading, variable interpolation
  daemon.rs    Daemon lifecycle, supervisor loop, IPC handlers
  model.rs     Core types (ProcessStatus, HealthProbe, etc.)
  ipc.rs       Request/Response protocol, socket helpers
  output.rs    JSON/table output formatting
  paths.rs     XDG path management, instance ID generation

tests/         Integration tests (spawn the binary end-to-end)
examples/      Sample compose files
assets/        Logo and static assets
.github/       CI and release workflows
flake.nix      Nix dev shell + package
```

## Build, test, lint

All commands are expected to pass locally and in CI:

```bash
cargo build --locked --all-targets
cargo test --locked --all-targets
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo doc --locked --no-deps
```

Use `nix develop` to get a toolchain-pinned shell if you prefer.

## Coding style

- Follow `rustfmt` defaults. Run `cargo fmt --all` before committing.
- Keep `clippy` clean under `-D warnings`. If a lint genuinely doesn't apply, use `#[allow(clippy::lint_name)]` with a comment explaining why.
- Prefer `anyhow::Result` at CLI/daemon boundaries, structured error types inside modules when useful.
- Keep CLI output compatible with the docker compose UX where sensible (see `CLAUDE.md`).

## Commit messages

**This repo uses [Conventional Commits](https://www.conventionalcommits.org/).** Every commit subject line MUST follow:

```
<type>(<optional scope>): <subject>
```

Types:

| Type       | Use for                                                 |
|------------|---------------------------------------------------------|
| `feat`     | A new user-facing feature                               |
| `fix`      | A bug fix                                               |
| `perf`     | A code change that improves performance                 |
| `refactor` | A code change that neither fixes a bug nor adds a feature |
| `test`     | Adding or updating tests                                |
| `docs`     | Documentation only (README, rustdoc, AGENTS.md, etc.)   |
| `style`    | Formatting/whitespace only (no code change)             |
| `build`    | Build system, `Cargo.toml`, `flake.nix`, dependencies   |
| `ci`       | CI configuration (`.github/workflows/*`)                |
| `chore`    | Other maintenance that doesn't fit above                |
| `revert`   | Reverts a previous commit                               |

Rules:

- Subject line: imperative mood ("add", not "added"), lowercase after the colon, no trailing period, ≤72 chars.
- Use a scope when it clarifies: `feat(daemon): add healthcheck retry backoff`, `ci(release): cache cargo registry`.
- Breaking changes: append `!` after type/scope (`feat!: drop --legacy flag`) AND include a `BREAKING CHANGE:` footer.
- Body (optional): wrap at ~72 cols, explain the *why*, reference issues (`Closes #123`, `Refs #456`).

Examples:

```
feat(cli): add --session flag for explicit environment naming
fix(daemon): prevent double-shutdown on SIGTERM race
refactor(config): extract env interpolation into its own module
docs: document readiness probe schema
ci: cache ~/.cargo/registry across jobs
chore(deps): bump tokio to 1.44
```

## Dependencies

- Deps in `Cargo.toml` are pinned to major versions (`"1"`, `"0.10"`, etc.) so `cargo update` stays deterministic via `Cargo.lock`.
- Avoid wildcard (`"*"`) specifiers — crates.io rejects them at publish time.
- When adding a dep, note the reason in the commit message.

## Release process

1. Ensure `main` is green.
2. Bump `version` in `Cargo.toml` (semver: breaking → major, new feature → minor, fix → patch).
3. Optionally update `CHANGELOG.md` — if present, the release workflow will embed the matching section in the GitHub Release notes.
4. Commit: `chore(release): v0.x.y`.
5. Tag: `git tag v0.x.y && git push --tags`.
6. The `release` workflow will:
   - Create a GitHub Release
   - Build and attach binaries for `{x86_64,aarch64}-{linux-gnu,apple-darwin}`
   - Publish to crates.io

## Secrets required

- `CARGO_REGISTRY_TOKEN` — crates.io API token. Trusted publishing is
  configured, so this can be a short-lived OIDC-exchanged token or a
  manual API token set in GitHub repository secrets.
- `GITHUB_TOKEN` — provided automatically by GitHub Actions.

## When using Claude / AI assistants

- Read `CLAUDE.md` for project-specific context before making non-trivial changes.
- Prefer small, focused commits that each pass CI on their own.
- Don't refactor unrelated code "while you're there" — open a separate PR.
- Don't rewrite already-pushed history unless explicitly asked.

<!-- BEGIN BEADS INTEGRATION v:1 profile:minimal hash:ca08a54f -->
## Beads Issue Tracker

This project uses **bd (beads)** for issue tracking. Run `bd prime` to see full workflow context and commands.

### Quick Reference

```bash
bd ready              # Find available work
bd show <id>          # View issue details
bd update <id> --claim  # Claim work
bd close <id>         # Complete work
```

### Rules

- Use `bd` for ALL task tracking — do NOT use TodoWrite, TaskCreate, or markdown TODO lists
- Run `bd prime` for detailed command reference and session close protocol
- Use `bd remember` for persistent knowledge — do NOT use MEMORY.md files

## Session Completion

**When ending a work session**, you MUST complete ALL steps below. Work is NOT complete until `git push` succeeds.

**MANDATORY WORKFLOW:**

1. **File issues for remaining work** - Create issues for anything that needs follow-up
2. **Run quality gates** (if code changed) - Tests, linters, builds
3. **Update issue status** - Close finished work, update in-progress items
4. **PUSH TO REMOTE** - This is MANDATORY:
   ```bash
   git pull --rebase
   bd dolt push
   git push
   git status  # MUST show "up to date with origin"
   ```
5. **Clean up** - Clear stashes, prune remote branches
6. **Verify** - All changes committed AND pushed
7. **Hand off** - Provide context for next session

**CRITICAL RULES:**
- Work is NOT complete until `git push` succeeds
- NEVER stop before pushing - that leaves work stranded locally
- NEVER say "ready to push when you are" - YOU must push
- If push fails, resolve and retry until it succeeds
<!-- END BEADS INTEGRATION -->
