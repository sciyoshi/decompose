Output ../../assets/tui-demo.gif

Set Shell "bash"
Set FontSize 14
Set Width 1200
Set Height 720
Set Padding 20
Set Theme "Catppuccin Mocha"
Set TypingSpeed 40ms

Env PS1 "$ "

# 1. Start the stack detached, then tail colored logs live.
Type "decompose up -d"
Enter
Sleep 1500ms
Type "decompose logs -f"
Enter
Sleep 6s
Ctrl+C
Sleep 1200ms

# 2. ps: the same fleet as a snapshot table.
Type "clear"
Enter
Sleep 300ms
Type "decompose ps"
Enter
Sleep 3s

# 3. Lifecycle commands: stop, start, restart a single service.
Type "decompose stop api"
Enter
Sleep 1500ms
Type "decompose ps"
Enter
Sleep 2500ms
Type "decompose start api"
Enter
Sleep 1500ms
Type "decompose restart worker"
Enter
Sleep 2s
Type "decompose ps"
Enter
Sleep 2500ms

# 4. The TUI: interactive view of the same fleet.
Type "clear && decompose tui"
Enter
Sleep 3500ms
Down
Sleep 700ms
Down
Sleep 700ms
Down
Sleep 1s
Tab
Sleep 2500ms

# Shift-Q stops services and exits the TUI.
Type "Q"
Sleep 3500ms
