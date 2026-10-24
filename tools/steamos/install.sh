#!/bin/sh
# Install sindenrs for the deck user: the binary in ~/.local/bin and a systemd user service
# (not enabled; `systemctl --user enable --now sindenrs` starts it with every session).
# Everything lives in $HOME, so SteamOS updates leave it alone.
set -eu
cd "$(dirname "$0")/../.."
bin=target/x86_64-unknown-linux-musl/release/sindenrs
[ -x "$bin" ] || tools/steamos/build.sh
install -Dm755 "$bin" "$HOME/.local/bin/sindenrs"
install -Dm644 tools/steamos/sindenrs.service "$HOME/.config/systemd/user/sindenrs.service"
systemctl --user daemon-reload
[ -e /etc/udev/rules.d/70-sinden-lightgun.rules ] ||
    echo "udev rules missing: sudo tools/steamos/install-udev.sh"
"$HOME/.local/bin/sindenrs" --version
