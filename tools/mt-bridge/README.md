# mt-bridge

Use the Mac's trackpad as a real multitouch device inside the om_wm dev VM.

UTM/QEMU only forwards pointer, click, and scroll to a guest, so the Mac
trackpad shows up there as a plain mouse and om_wm's custom gesture engine
(`touch.rs`, `trackpad_mode = custom`) has no multitouch to read. mt-bridge fills
that gap: it reads raw per-finger data on the host and synthesizes a matching
`uinput` touchpad in the guest, so `touch.rs` sees genuine protocol-B multitouch.

    Mac trackpad
      -> mt-capture (host, MultitouchSupport.framework)
      -> TCP over the vmnet link (host 192.168.64.1)
      -> mt-inject (guest, creates a uinput touchpad)
      -> libinput classifies it -> om_wm touch.rs

The guest device mimics an Apple trackpad (axis span ~9760x6750, clickpad, the
`BTN_TOOL_*TAP` finger-count bits), which is what `touch.rs` was tuned against, so
its gesture thresholds keep their intended feel.

See `protocol.md` for the wire format.

## Build

Host (on the Mac):

    ./host/build.sh          # -> ./host/mt-capture

Guest (in the VM, source is read-only on the 9p share so it builds to $HOME):

    bash guest/build.sh      # -> ~/mt-inject

`mt-capture` links Apple's private MultitouchSupport.framework, so an OS update
can break it; that is the only way to get trackpad multitouch on macOS.

## Run

1. Guest: create the touchpad and connect back to the host.

       ~/mt-inject                       # connects to 192.168.64.1:9999
       ~/mt-inject --test                # synthetic pan/pinch, no host needed

2. Host: start streaming (bind to the vmnet host IP).

       ./host/mt-capture                 # listens on 192.168.64.1:9999

   Run this in a real terminal (Terminal.app or iTerm), not inside an editor's
   embedded terminal: the click tap needs Input Monitoring for the app that owns
   the process, and MultitouchSupport only delivers touches while that app is
   frontmost. Grant the terminal Input Monitoring (System Settings > Privacy &
   Security > Input Monitoring) and restart it once; the startup line then reads
   "click tap active".

3. Guest: start om_wm *after* mt-inject, so it finds the touchpad at startup.

       cd ~/repos/om_wm && ~/run_om_tty.sh

Now touch the Mac trackpad while the mt-capture terminal is frontmost, and watch
the (unfocused) UTM window: two fingers pan and pinch the canvas, one finger moves
the pointer, a click clicks. Press Enter in the mt-capture terminal to pause or
resume forwarding.

macOS gestures: om_wm uses 3- and 4-finger gestures, and so does macOS (Mission
Control, App Exposé, swipe between full-screen apps and spaces, three-finger tap).
Both fire at once, since macOS handles those below where any tap could swallow
them. Turn them off while you work in the VM: System Settings > Trackpad, uncheck
Mission Control, App Exposé, "Swipe between full-screen apps," and (under Point &
Click) the three-finger tap; re-enable them afterward.

Focus rules that fall out of how macOS delivers multitouch:

- Touches reach the guest only while the mt-capture terminal is the frontmost app.
  Focus anything else and the trackpad is a normal Mac trackpad again.
- You cannot gate it on the UTM window instead: when UTM is frontmost it grabs the
  trackpad exclusively and MultitouchSupport delivers nothing. So om_wm renders to
  the console and is driven through the bridge with UTM in the background.

## Permissions and setup notes (already done in this VM)

- `/dev/uinput` is group `input`, mode `0660`, via
  `/etc/udev/rules.d/99-uinput.rules`, and `uinput` autoloads via
  `/etc/modules-load.d/uinput.conf`. So `mt-inject` needs no root, since the user
  is in the `input` group.
- The host binds `192.168.64.1`, the vmnet bridge address, which exists while the
  VM runs. If the VM's network changes, pass another bind address:
  `./host/mt-capture 0.0.0.0 9999` and point `mt-inject --connect` at it.

## Known gaps / next steps

- Clicks are streamed via a CGEventTap (left and right mouse buttons). Needs Input
  Monitoring for the terminal, see above.
- `size`/pressure scaling (`majorAxis / 10`) is a first guess; tune against how
  `touch.rs` reads contact size and whole-pad load.
- Y is flipped in the guest (Mac origin bottom-left, Linux MT top-left). If pan
  feels inverted, that is the knob.
