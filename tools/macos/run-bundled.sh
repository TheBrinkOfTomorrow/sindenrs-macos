#!/bin/sh
# Run sindenrs from its app bundle (tools/macos/bundle.sh), so macOS gives it its own camera
# grant instead of judging it by the app that launched it (docs/macos-notes.md, "Camera
# permission"). Output is printed when it exits.
#
#   tools/macos/run-bundled.sh [--bin path] -- <args...>     (default bin: target/debug/sindenrs)
#
# LaunchServices starts the app with / as its working directory: pass absolute paths.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
bin="$root/target/debug/sindenrs"
if [ "${1:-}" = "--bin" ]; then bin=$2; shift 2; fi
if [ "${1:-}" = "--" ]; then shift; fi
app=$("$here/bundle.sh" "$bin")
log="$root/target/macos/sindenrs.log"
rm -f "$log"
open -W -n "$app" --stdout "$log" --stderr "$log" --args "$@"
cat "$log"
