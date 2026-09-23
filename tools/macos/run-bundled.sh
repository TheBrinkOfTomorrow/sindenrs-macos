#!/bin/sh
# Run a binary (default: target/debug/sindenrs) as an ad-hoc signed app bundle, so macOS gives
# it its own camera grant instead of judging it by the app that launched it (see
# docs/macos-notes.md, "Camera permission"). Output is printed when it exits.
#
#   tools/macos/run-bundled.sh [--bin path] -- <args...>
#
# LaunchServices starts the app with / as its working directory: pass absolute paths.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
bin="$root/target/debug/sindenrs"
if [ "${1:-}" = "--bin" ]; then bin=$2; shift 2; fi
if [ "${1:-}" = "--" ]; then shift; fi
name=$(basename "$bin")
app="$root/target/macos/$name.app"
mkdir -p "$app/Contents/MacOS"
sed -e "s#dev.sindenrs.probe#dev.sindenrs.$name#" -e "s#SindenProbe#$name#" \
    -e "s#<string>grab</string>#<string>$name</string>#" "$here/Info.plist" > "$app/Contents/Info.plist"
cp "$bin" "$app/Contents/MacOS/$name"
codesign -s - --force "$app" 2>/dev/null
log="$root/target/macos/$name.log"
rm -f "$log"
open -W -n "$app" --stdout "$log" --stderr "$log" --args "$@"
cat "$log"
