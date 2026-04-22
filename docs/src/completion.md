# Shell completion

`decompose completion <shell>` prints a completion script to stdout for one
of `bash`, `zsh`, `fish`, `powershell`, or `elvish`. On `bash`, `zsh`,
`fish`, and PowerShell, the script also does dynamic completion: service
names for `start`, `stop`, `restart`, `kill`, `logs`, `exec`, `run`, and
`up` are pulled from `decompose config --json`, and `--session` /
`--project-name` values are pulled from `decompose ls --json`. The helpers
forward any `--file`, `-e`/`--env-file`, `--session`/`--project-name`, and
`--disable-dotenv` flags already on the command line so completion stays
correct in multi-project and multi-session setups. `jq` is optional but
recommended; without it, a `sed` fallback parses the JSON.

## bash

Source for the current shell:

```sh
source <(decompose completion bash)
```

Install system-wide (requires `bash-completion`):

```sh
decompose completion bash | sudo tee /etc/bash_completion.d/decompose > /dev/null
```

## zsh

Write the script to a directory on your `$fpath`. You must have a writeable
entry, for example `~/.zfunc`, added before `compinit` runs:

```sh
mkdir -p ~/.zfunc
decompose completion zsh > ~/.zfunc/_decompose
```

Then in `~/.zshrc`, before `compinit`:

```sh
fpath+=(~/.zfunc)
autoload -U compinit && compinit
```

Or source it directly from `~/.zshrc`:

```sh
source <(decompose completion zsh)
```

## fish

```sh
decompose completion fish > ~/.config/fish/completions/decompose.fish
```

## PowerShell

One-shot in the current session:

```pwsh
decompose completion powershell | Out-String | Invoke-Expression
```

Or add the same line to your `$PROFILE` to load it on every shell start.

## elvish

Write the script to a module file and `use` it from `rc.elv`:

```sh
decompose completion elvish > ~/.config/elvish/lib/decompose.elv
```

Then add `use decompose` to `~/.config/elvish/rc.elv`.
