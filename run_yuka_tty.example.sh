#!/bin/sh
#
# Example KMS/TTY launcher for the yuka dev VM.
#
# The repo's run_tty.sh assumes a writable in-tree target/ and picks the
# connector's current mode. In a VM the checkout is often a read-only 9p share
# and the virtual GPU lands on a small fallback mode, so this wrapper sets the
# few machine-specific knobs and adds two console-safety measures. Copy it to
# somewhere writable (e.g. ~/run_yuka_tty.sh), edit the three knobs below, and
# run that copy. It is an example on purpose: the paths and the display size are
# specific to one machine and do not belong in version control.
#
#   ~/run_yuka_tty.sh            no frame limit (runs until you quit / pkill it)
#   ~/run_yuka_tty.sh 600        bounded: stop after 600 frames (~10s)
#   YUKA_SIZE=2560x1440 ~/run_yuka_tty.sh   override the mode for one run
#
# The two safety measures:
#  - stdin from /dev/null so raylib does not grab the tty keyboard (logind's K_OFF
#    already handles the console; input comes through libinput either way). Without
#    this, a run started ON the console leaves the tty keyboard dead on exit.
#  - an EXIT trap that force-restores the keyboard, covering crashes and kills too.

#
# Machine-specific knobs: edit these three for your VM.
#

# Build output goes here rather than in-tree, since the 9p source share is read-only.
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/.cache/yuka-target}"
# Where the checkout is mounted.
checkout="$HOME/repos/yuka"
# The connector mode to request. yuka picks this exact mode; without it a virtual
# GPU can land on a tiny fallback. Must be a mode the connector actually lists
# (see /sys/class/drm/card*-*/modes). Any run can override it: YUKA_SIZE=WxH ...
: "${YUKA_SIZE:=1920x1080}"; export YUKA_SIZE

export PATH="$HOME/.cargo/bin:$PATH"
cd "$checkout" || exit 1

vt=$(sed 's/tty//' /sys/class/tty/tty0/active 2>/dev/null)
restore_kbd() { [ -n "$vt" ] && sudo kbd_mode -u -f -C "/dev/tty${vt}" 2>/dev/null; }
trap restore_kbd EXIT INT TERM

frames=${1:-}
log=${YUKA_LOG_FILE:-/tmp/yuka-vt${vt:-x}.txt}
bin="$CARGO_TARGET_DIR/debug/yuka"

if [ ! -x "$bin" ]; then
    echo "yuka: building ($bin)"
    cargo build || exit 1
fi

echo "yuka: vt${vt:-?}, frames=${frames:-unlimited}, ${YUKA_SIZE}, log $log"
if [ -n "$frames" ]; then
    YUKA_MAX_FRAMES="$frames" "$bin" > "$log" 2>&1 < /dev/null
else
    "$bin" > "$log" 2>&1 < /dev/null
fi
status=$?
echo
echo "--- $log (exit $status)"
grep -iE "session control|through libseat|graphic device fd|connector mode|window \+|no usable|no keyboard|failed|master|shutting down" "$log" || true
