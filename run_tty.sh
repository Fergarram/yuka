#!/bin/sh
#
# A bounded om_wm run, for testing on a tty other than the usual one.
#
# Two things make this safe to run from a fresh tty. It stops by itself after a
# frame count, so a run that renders nothing and swallows the keyboard still ends
# without a reboot. And the log goes to a file, so it survives the tty it was
# started from: a stuck run used to take its own output with it.
#
# Usage:
#   ./run_tty.sh                      600 frames, about ten seconds
#   ./run_tty.sh 1800                 longer, for actually interacting with it
#   OM_WM_CARD=raylib ./run_tty.sh    raylib opens the card, instead of libseat
#
# If it does lock up: ctrl+alt+F1 gets you back, and pkill -x om_wm from there.
#

cd "$(dirname "$0")" || exit 1

frames=${1:-600}
vt=$(sed 's/tty//' /sys/class/tty/tty0/active 2>/dev/null)
log=${OM_WM_LOG_FILE:-/tmp/om_wm-vt${vt:-x}.txt}
bin=./target/debug/om_wm

if [ ! -x "$bin" ]; then
    echo "om_wm: no $bin yet, building"
    cargo build || exit 1
fi

echo "om_wm: vt${vt:-?}, $frames frames, log $log"
OM_WM_MAX_FRAMES="$frames" "$bin" > "$log" 2>&1
status=$?

# The lines worth reading first: which vt and session we got, where the card came
# from, whether any client actually mapped a window, and how it ended.
echo
echo "--- $log (exit $status)"
grep -iE "session control|through libseat|graphic device fd|window \+|no usable|no keyboard|failed|master|shutting down" "$log" || true
