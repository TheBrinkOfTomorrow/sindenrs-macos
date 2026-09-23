#!/bin/sh
# Build camera_grab.swift into an ad-hoc signed app bundle and launch it through LaunchServices.
#
# Camera access (TCC) is granted per app, and an app that does not declare
# NSCameraUsageDescription is refused without a prompt. A bare binary inherits the permission of
# whatever app launched it (Claude, an IDE), so give it a bundle of its own and `open` it.
# Output goes to $OUT (default: a temp dir).
set -eu
here=$(cd "$(dirname "$0")" && pwd)
OUT=${OUT:-$(mktemp -d)}
app="$OUT/SindenProbe.app"
mkdir -p "$app/Contents/MacOS"
cp "$here/Info.plist" "$app/Contents/Info.plist"
swiftc -O "$here/camera_grab.swift" -o "$app/Contents/MacOS/grab"
codesign -s - --force "$app"
open -W -n "$app" --stdout "$OUT/probe.log" --stderr "$OUT/probe.log" --args "$OUT/frame.pgm"
cat "$OUT/probe.log"
echo "frame: $OUT/frame.pgm"
