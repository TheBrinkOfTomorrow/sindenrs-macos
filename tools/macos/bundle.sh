#!/bin/sh
# Package a sindenrs binary as target/macos/Sindenrs.app and sign it.
#
#   tools/macos/bundle.sh [path/to/sindenrs]     (default: target/release/sindenrs)
#
# macOS grants the camera per app and remembers the grant by the app's code signature. An
# ad-hoc signature changes with every build, so macOS may ask again after each rebuild; a
# fixed signing identity keeps the grant. The identity is $SINDENRS_SIGN_IDENTITY, default
# "sindenrs dev" (a self-signed Code Signing certificate made in Keychain Access); without it
# the app is signed ad hoc, with a warning. Prints the app's path.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
bin=${1:-"$root/target/release/sindenrs"}
identity=${SINDENRS_SIGN_IDENTITY:-sindenrs dev}
[ -x "$bin" ] || { echo "no binary at $bin; build it first (cargo build --release)" >&2; exit 1; }

version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)
app="$root/target/macos/Sindenrs.app"
mkdir -p "$app/Contents/MacOS"
sed "s/@VERSION@/$version/g" "$here/Sindenrs-Info.plist" > "$app/Contents/Info.plist"
cp "$bin" "$app/Contents/MacOS/sindenrs"

# Self-signed certificates are not "valid" (untrusted) but sign fine, so look them up in the
# full list rather than with -v.
if security find-identity -p codesigning | grep -q "\"$identity\""; then
    codesign --force --sign "$identity" "$app" 2>/dev/null
else
    echo "warning: no code-signing identity \"$identity\"; signing ad hoc (the camera grant may not survive rebuilds)" >&2
    codesign --force --sign - "$app" 2>/dev/null
fi
echo "$app"
