Output ../../assets/tui-demo.gif

Set Shell "bash"
Set FontSize 14
Set Width 1200
Set Height 720
Set Padding 20
Set Theme "Catppuccin Mocha"
Set TypingSpeed 35ms

Env PS1 "$ "

# Start the demo stack. Visible in the gif — it's a quick, honest look
# at `up -d` landing before we drop into the TUI.
Type "decompose up -d"
Enter
Sleep 2500ms

Type "clear && decompose tui"
Enter
Sleep 3500ms

# Browse the process list.
Down
Sleep 600ms
Down
Sleep 600ms
Down
Sleep 1s

# Switch focus to the log pane.
Tab
Sleep 1200ms

# Regex search with highlighting and n/N navigation.
Type "/"
Sleep 400ms
Type "ERROR"
Sleep 600ms
Enter
Sleep 1800ms
Type "n"
Sleep 1200ms
Type "n"
Sleep 1200ms
Escape
Sleep 800ms

# Shift-Q stops services and exits the TUI.
Type "Q"
Sleep 3500ms
