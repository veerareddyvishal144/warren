# Warren demo: a macOS instance for a coding agent, recorded with VHS
# (https://github.com/charmbracelet/vhs). Install: brew install vhs
# Run against a daemon that already has the warren-macos base VM:
#   WARREN_API_URL=http://127.0.0.1:7347 WARREN_CONFIG=~/.config/warren/config.json vhs scripts/demo/warren-demo.tape
# Output: docs/assets/warren-demo.gif (about 90 seconds of real time).
Output docs/assets/warren-demo.gif
Set Shell zsh
Set FontSize 15
Set Width 1200
Set Height 700
Set Theme "Catppuccin Mocha"
Set TypingSpeed 40ms
Set Padding 16

Hide
Type "export WARREN_API_URL=${WARREN_API_URL:-http://127.0.0.1:7347}; clear"
Enter
Show

Type "# Warren: a real macOS computer for a coding agent, on the Mac I already own"
Enter
Sleep 1.5s
Type "warren instances create --template warren-macos --wait"
Enter
Sleep 30s
Type "ID=$(warren --json instances list | jq -r '.data[0].id'); echo $ID"
Enter
Sleep 1.5s
Type "warren instances exec $ID -- 'sw_vers; whoami; nohup sleep 100000 >/dev/null 2>&1 & echo marker pid $!'"
Enter
Sleep 4s
Type "warren screenshot $ID -o /tmp/desktop.png && file /tmp/desktop.png"
Enter
Sleep 3s
Type "warren input $ID key cmd+space && warren input $ID type Terminal && warren input $ID key enter"
Enter
Sleep 4s
Type "warren screenshot $ID -o /tmp/terminal.png && file /tmp/terminal.png"
Enter
Sleep 3s
Type "# sleep keeps memory and processes; wake brings them back with the same pid"
Enter
Type "warren instances sleep $ID && warren instances start $ID"
Enter
Sleep 16s
Type "warren instances exec $ID -- 'pgrep -f \"sleep 100000\"'"
Enter
Sleep 3s
Type "warren instances delete $ID"
Enter
Sleep 3s
