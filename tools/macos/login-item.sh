#!/bin/sh
# Start `sindenrs run` at login, from ~/Applications/Sindenrs.app, as a per-user launchd agent.
#
#   tools/macos/login-item.sh install     copy the app (tools/macos/bundle.sh) and load the agent
#   tools/macos/login-item.sh uninstall   unload and remove the agent (the app stays)
#   tools/macos/login-item.sh status
#
# launchd restarts `run` if it exits with an error. Output goes to ~/Library/Logs/sindenrs.log.
# The first start asks for camera access for Sindenrs; a fixed signing identity (see
# bundle.sh) keeps that grant across updates.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
label=dev.sindenrs.run
agent="$HOME/Library/LaunchAgents/$label.plist"
app="$HOME/Applications/Sindenrs.app"
domain="gui/$(id -u)"

case "${1:-}" in
install)
    built=$("$here/bundle.sh")
    mkdir -p "$HOME/Applications" "$HOME/Library/LaunchAgents" "$HOME/Library/Logs"
    launchctl bootout "$domain/$label" 2>/dev/null || true
    rm -rf "$app"
    cp -R "$built" "$app"
    cat > "$agent" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>Label</key><string>$label</string>
<key>ProgramArguments</key><array>
    <string>$app/Contents/MacOS/sindenrs</string>
    <string>run</string>
</array>
<key>RunAtLoad</key><true/>
<key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>
<key>ProcessType</key><string>Interactive</string>
<key>StandardOutPath</key><string>$HOME/Library/Logs/sindenrs.log</string>
<key>StandardErrorPath</key><string>$HOME/Library/Logs/sindenrs.log</string>
</dict></plist>
PLIST
    launchctl bootstrap "$domain" "$agent"
    echo "installed $app and started $label; log: ~/Library/Logs/sindenrs.log"
    ;;
uninstall)
    launchctl bootout "$domain/$label" 2>/dev/null || true
    rm -f "$agent"
    echo "removed $label (the app stays at $app)"
    ;;
status)
    launchctl print "$domain/$label" 2>/dev/null | grep -E '^\s*(state|pid|last exit code)' || echo "$label is not loaded"
    ;;
*)
    echo "usage: $0 install|uninstall|status" >&2
    exit 2
    ;;
esac
