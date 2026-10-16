#!/bin/sh
# Bench the serial-write wedge hands-free: one gun, its camera in-process (as in `run`), 900
# frames (~15 s), the centre position streamed from the first frame. Prints CLEAN or WEDGED,
# then how the port close went.
#
#   tools/macos/bench-wedge.sh gun|driver [masked|unmasked]
#
#   gun     the gun's own HID pointer is the cursor (no Core Graphics, no permissions)
#   driver  the driver warps the cursor and synthesises clicks (today's default)
#   masked / unmasked   override whether the gun's HID mouse buttons are masked (default:
#           masked for driver, unmasked for gun), to separate the two differences
#
# The gun can lie on the desk; no border is needed. Build first (cargo build). Replug the gun
# after any WEDGED run before the next one: a wedged gun stays wedged. SINDEN_CAMERA=<uniqueID>
# names the camera explicitly (right after a previous run the hub-powered camera can be off the
# bus for a second or more, and discovery misses it).
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
usage="usage: bench-wedge.sh gun|driver [masked|unmasked]"
cursor=${1:?$usage}
case "$cursor" in gun|driver) ;; *) echo "$usage" >&2; exit 2;; esac
mask=${2:-}
set -- debug track --send --heartbeat --frames 900 --cursor "$cursor"
case "$mask" in
    masked) set -- "$@" --mask-buttons true ;;
    unmasked) set -- "$@" --mask-buttons false ;;
    "") ;;
    *) echo "$usage" >&2; exit 2 ;;
esac
if [ -n "${SINDEN_CAMERA:-}" ]; then set -- "$@" --device "$SINDEN_CAMERA"; fi

mkdir -p "$root/target/macos/logs"
log="$root/target/macos/logs/bench-$cursor${mask:+-$mask}-$(date +%Y%m%d-%H%M%S).log"
echo "bench: $* (log: $log)"
SINDENRS_LOG=$log "$here/run-bundled.sh" -- "$@" > /dev/null 2>&1 &
runner=$!

# Wait for the run to finish, but not for the process to exit: if the port close is stuck in
# the kernel, the process hangs on its way out and would never let `open -W` return.
deadline=$(( $(date +%s) + 90 ))
while [ "$(date +%s)" -lt "$deadline" ]; do
    if [ -f "$log" ] && grep -q -E "^frames=|Error:" "$log"; then
        sleep 2
        break
    fi
    if ! kill -0 "$runner" 2>/dev/null; then break; fi
    sleep 1
done

strip() { sed 's/\x1b\[[0-9;]*m//g' "$log"; }
if strip | grep -q "position write timed out"; then
    echo "WEDGED: $(strip | grep -m1 "position write timed out")"
    echo "  first report lines:"; strip | grep -m3 "proc=" | sed 's/^/  /'
elif strip | grep -q "^frames="; then
    echo "CLEAN: no position write timed out"
else
    echo "NO VERDICT: the run ended without a summary; last lines:"
    strip | tail -8 | sed 's/^/  /'
fi
strip | grep -m1 "^frames=" | sed 's/^/  /' || true
# The last close is the tracker's own (a recovery reset during connect closes the port too).
last_close=$(strip | grep -E "closed gun serial port|stuck in the kernel" | tail -1)
if echo "$last_close" | grep -q "stuck in the kernel"; then
    echo "close: STUCK in the kernel (the non-blocking close did not help) — replug the gun"
elif [ -n "$last_close" ]; then
    echo "close: $(echo "$last_close" | sed 's/.*took=/took /')"
else
    echo "close: not logged (set [global] log = \"debug\" to see the close timing)"
fi
if kill -0 "$runner" 2>/dev/null; then
    echo "note: the app has not exited yet; if it never does, its serial close is stuck — replug the gun"
fi
