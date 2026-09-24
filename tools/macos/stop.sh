#!/bin/sh
# Stop a running sindenrs the way Ctrl-C does (SIGINT), so it shuts down cleanly: trackers
# stop, the overlay and the menu bar item go away. The same as the menu's Quit or ⌃⌥⌘Q.
if pkill -INT -x sindenrs; then
    echo "stopping sindenrs"
else
    echo "sindenrs is not running"
fi
