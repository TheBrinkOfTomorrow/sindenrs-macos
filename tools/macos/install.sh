#!/bin/sh
# Build Sindenrs.app (release, signed; see bundle.sh) and copy it to /Applications, or to the
# folder given. Opening the app starts `sindenrs run`; quit it from its menu bar item, with
# ⌃⌥⌘Q, or with tools/macos/stop.sh. Its log is ~/Library/Logs/Sindenrs.log.
#
#   tools/macos/install.sh [destination folder]      (default: /Applications)
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
dest=${1:-/Applications}
(cd "$root" && cargo build --release)
app=$("$here/bundle.sh" "$root/target/release/sindenrs")
if pgrep -x sindenrs >/dev/null; then
    echo "sindenrs is running; quit it first (⌃⌥⌘Q)" >&2
    exit 1
fi
rm -rf "$dest/Sindenrs.app"
cp -R "$app" "$dest/Sindenrs.app"
echo "installed $dest/Sindenrs.app"
