# Terminal UI

Open the terminal UI for a running project:

```sh
decompose tui
```

Use the same `--file` or `--session` options you used to start the environment.
You can also start it and open the UI with `decompose up --tui`.

The upper pane lists process state; the lower pane shows logs for the selected
process (only that replica when a service has multiple replicas). Selecting a
process loads its recent logs and resumes following new output. Search and
copying use this filtered buffer. Press Tab to switch pane focus.

## Leaving and stopping

In normal mode, **`q` or Ctrl-C detaches**, leaving the daemon and its services
running. **Uppercase `Q` shuts down the environment**, like `decompose down`,
then exits the UI. While shutdown is in progress, pressing `Q` again or
Ctrl-C forces shutdown.

While entering a search, press Enter or Esc first to return to normal mode.

## Keybindings

These keys work in normal mode with either pane focused:

| Key | Action |
|-----|--------|
| Tab | Switch between the process list and logs. |
| `j` / Down, `k` / Up | Select the next or previous process. |
| `g`, `G` | Select the first or last process. |
| `u` | Start the selected service. |
| `s` | Stop the selected service. |
| `r` | Restart the selected service. |
| Page Up, Page Down | Scroll logs by ten lines toward older or newer output. |
| `/` | Enter a regular expression to search the buffered logs. |
| `n`, `N` | Jump to the next or previous match, wrapping at the buffer ends. |
| Esc | Clear the search. |
| `q`, Ctrl-C | Detach from the UI. |
| `Q` | Stop the environment and exit. |

Service actions target the selected process's base service, including all
its replicas.

With the **logs pane focused**, these additional keys are available:

| Key | Action |
|-----|--------|
| Home | Jump to the oldest buffered output and pause following. |
| End | Jump to the newest output and resume following. |
| `p` | Toggle following new output. |
| `y` | Copy the visible log lines to the clipboard as plain text. |
| `Y` | Copy the entire buffered log to the clipboard as plain text. |

Scrolling toward older output or jumping to a search match pauses following.
Scrolling back to the newest output resumes it. Pausing keeps incoming logs
in the buffer; it only stops the view from following them. Clipboard copying
uses OSC 52 and requires support from your terminal.

## Search

Press `/`, type a regular expression, then press Enter to jump to a match.
Matches are highlighted; `n` moves toward newer output and `N` toward older
output. Backspace edits the expression while typing. Esc cancels search entry
or clears an active search. Normal shortcuts are suspended while entering
the expression.

Search and copying operate on the UI's in-memory buffer, not the complete log
history. Use `decompose logs` to inspect logs outside that buffer.
