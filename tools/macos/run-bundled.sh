#!/bin/sh
# Run sindenrs from its app bundle (tools/macos/bundle.sh), so macOS gives it its own camera
# grant instead of judging it by the app that launched it (docs/macos-notes.md, "Camera
# permission"). Output is printed when it exits.
#
#   tools/macos/run-bundled.sh [--bin path] -- <args...>     (default bin: target/debug/sindenrs)
#
# LaunchServices starts the app with / as its working directory: pass absolute paths.
#
# Each run logs to its own file under target/macos/logs/, and target/macos/sindenrs.log links
# to the latest. Earlier runs' logs are kept: a run that hung or was killed (so this script
# never got to print it) still has its output on disk after the next run starts.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
bin="$root/target/debug/sindenrs"
if [ "${1:-}" = "--bin" ]; then bin=$2; shift 2; fi
if [ "${1:-}" = "--" ]; then shift; fi
app=$("$here/bundle.sh" "$bin")
logs="$root/target/macos/logs"
latest="$root/target/macos/sindenrs.log"
mkdir -p "$logs"
# A plain file there is a log from before runs kept their own; keep it too.
if [ -f "$latest" ] && [ ! -L "$latest" ]; then
    mv "$latest" "$logs/sindenrs-$(stat -f %Sm -t %Y%m%d-%H%M%S "$latest").log"
fi
log="$logs/sindenrs-$(date +%Y%m%d-%H%M%S)-$$.log"
: > "$log"
ln -sfn "$log" "$latest"
echo "log: $log" >&2
open -W -n "$app" --stdout "$log" --stderr "$log" --args "$@"
cat "$log"
