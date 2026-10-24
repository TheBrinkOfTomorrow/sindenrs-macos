#!/bin/sh
# Install the udev rules on SteamOS. Run as root: sudo tools/steamos/install-udev.sh
#
# /etc survives SteamOS updates, so this is needed once. Arch has no `dialout` group; serial
# ports belong to `uucp`, so the rules are rewritten for it. TAG+="uaccess" already gives
# the logged-in deck user access in both Game Mode and Desktop Mode, so no group changes
# are needed.
set -eu
[ "$(id -u)" -eq 0 ] || { echo "run as root: sudo $0" >&2; exit 1; }
here="$(dirname "$0")/../../udev"
sed 's/GROUP="dialout"/GROUP="uucp"/; s/chgrp -f dialout/chgrp -f uucp/' \
    "$here/70-sinden-lightgun.rules" > /etc/udev/rules.d/70-sinden-lightgun.rules
# Tag the gun ID_INPUT_GUN for emulators that read evdev; leave it a normal mouse, which
# is what Steam Input and most emulators on the Deck expect.
grep -v '^# @HIDE@' "$here/71-sinden-lightgun-input.rules" \
    > /etc/udev/rules.d/71-sinden-lightgun-input.rules
udevadm control --reload
udevadm trigger
echo "installed /etc/udev/rules.d/70-sinden-lightgun.rules and 71-sinden-lightgun-input.rules"
