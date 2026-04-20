---
name: release
description: Cut a new decompose release. Bumps the Cargo.toml version, commits, then creates a GitHub release with notes generated from commits since the last tag. Pushing the release tag triggers the GitHub Actions workflow that uploads binaries and publishes to crates.io. GitHub releases are the source of truth for the changelog — there is no CHANGELOG.md.
---

You are helping the user cut a new release of `decompose`.

# Version format

The crate uses SemVer. Cargo rejects PEP 440 syntax (`0.1.0a1` is not valid). Use:

- Stable: `0.1.0`, `0.2.0`, `1.0.0`
- Pre-release: `0.1.0-alpha.1`, `0.1.0-beta.1`, `0.1.0-rc.1`

The git tag is always `v` + the version, e.g. `v0.1.0-alpha.1`.

# Args

The user invokes this skill with the target version as the argument, e.g. `/release 0.1.0-alpha.1`. If no argument is given, ask the user which version to cut before doing anything else. Validate the version is SemVer.

# Steps

Execute these in order. Stop and ask the user if anything is surprising.

## 1. Preflight

Run these checks in parallel. Abort if any fails.

```bash
git status --porcelain              # must be empty (clean tree)
git rev-parse --abbrev-ref HEAD     # must be "main"
git fetch origin main               # sync refs
git rev-list --count HEAD..origin/main  # must be 0 (up to date with remote)
git rev-list --count origin/main..HEAD  # must be 0 (no unpushed commits)
```

Check CI is green on the current HEAD:

```bash
gh run list --branch main --limit 1 --json conclusion,status,headSha
```

The most recent run on main must have `conclusion == "success"` and `headSha` matching current HEAD. If it's still running (`status == "in_progress"`), ask the user whether to wait or proceed. If it's failing, abort.

## 2. Determine version and previous tag

- Current version: read the `version = "..."` line from `Cargo.toml`.
- Target version: from the skill args (validated above).
- Previous tag: `git describe --tags --abbrev=0 2>/dev/null` — may be empty for the first release.

Confirm to the user: "Cutting release: current=`{current}`, target=`{target}`, previous tag=`{tag or none}`. Proceed?" Do not take any irreversible action until they confirm.

## 3. Bump Cargo.toml

Use Edit to change the `version = "..."` line in Cargo.toml to the target version. Then run `cargo build --locked` to refresh Cargo.lock. (If `--locked` fails because the lockfile needs updating, run `cargo build` without `--locked` once, then re-run with `--locked` to verify.)

Expected diff: 1 line in Cargo.toml, 1 line in Cargo.lock (the decompose entry).

## 4. Commit and push

```bash
git add Cargo.toml Cargo.lock
git commit -m "chore(release): v{target}"
git push origin main
```

Use a heredoc for the commit message format shown in CLAUDE.md. Include the Co-Authored-By trailer.

## 5. Generate release notes

```bash
git log --pretty=format:'- %s (%h)' {previous_tag}..HEAD
```

If there is no previous tag, use `git log --pretty=format:'- %s (%h)' HEAD` (entire history — probably too much; trim to recent commits by inspection).

Filter the result:

- **Exclude** the `chore(release): v{target}` commit just made (it's noise).
- **Exclude** merge commits (`Merge pull request ...`) unless they're the only signal.
- **Group** by type if the log is long: `### Features`, `### Fixes`, `### Refactors`, `### Tests`, `### CI/Build`. Use the Conventional Commits prefix to group.
- **Tidy** wording: strip the type prefix from each line (`feat(cli): add X` → `add X`). Keep the `(%h)` short SHA at the end.

Example output:

```markdown
## What's Changed

### Features
- add shell completion subcommand (47e03c8)
- emit restart separator lines between process runs (370eb1b)

### Fixes
- split ready and alive flags so both probes coexist (8ec2f18)

### Refactors
- drop ProcessSnapshot.healthy back-compat field (f012034)
- extract with_process_mut helper (810dd18)

**Full changelog**: https://github.com/sciyoshi/decompose/compare/{previous_tag}...v{target}
```

For the **first** release (no previous tag), use a short "Initial release." summary and optionally a list of top-level capabilities rather than every commit.

Show the generated notes to the user and ask them to approve or edit before creating the release.

## 6. Create the GitHub release (creates and pushes the tag)

```bash
gh release create v{target} \
  --title "v{target}" \
  --notes "$(cat <<'EOF'
{notes_body}
EOF
)" \
  --target main
```

For pre-releases (`-alpha`, `-beta`, `-rc` in the version), add `--prerelease`.

This command creates both the GitHub release AND pushes the git tag `v{target}` to the remote. The tag push triggers `.github/workflows/release.yml`, which uploads binaries and publishes to crates.io.

## 7. Watch the workflow

Report to the user:
- The release URL (`gh release view v{target} --web` to open, or just the URL).
- The workflow run to watch (`gh run watch` or paste the URL from `gh run list --limit 1`).
- That crates.io publishing typically takes 2–3 minutes after the workflow starts.

# Aborts and recovery

- If the Cargo.toml bump or push fails before the tag is created: just reset. `git reset --hard origin/main` then try again.
- If `gh release create` fails after the push has already landed: the commit is on main but no tag exists. Re-run the `gh release create` command — the release commit is a legitimate part of main even without a tag.
- If the release was created but the workflow failed: fix the issue, then either delete+recreate the release or push a new tag.
- If crates.io publishing fails (e.g. version already exists): it's a hard error — crates.io does not allow republishing. Cut a new version.

# What this skill intentionally does NOT do

- Does not generate or maintain a CHANGELOG.md. The GitHub release notes are the changelog.
- Does not decide the target version automatically. The user chooses it.
- Does not re-run local tests. CI is authoritative; we verify CI is green in step 1 instead.
- Does not skip crates.io publishing. If you want to skip, edit `.github/workflows/release.yml` temporarily, or don't push the tag.
