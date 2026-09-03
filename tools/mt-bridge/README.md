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

3. Guest: start om_wm *after* mt-inject, so it finds the touchpad at startup.

       cd ~/repos/om_wm && ~/run_om_tty.sh

Now touch the Mac trackpad: two fingers pan and pinch the canvas, and the rest of
`touch.rs` (taps, pointer, gestures) is driven by the real pad.

## Permissions and setup notes (already done in this VM)

- `/dev/uinput` is group `input`, mode `0660`, via
  `/etc/udev/rules.d/99-uinput.rules`, and `uinput` autoloads via
  `/etc/modules-load.d/uinput.conf`. So `mt-inject` needs no root, since the user
  is in the `input` group.
- The host binds `192.168.64.1`, the vmnet bridge address, which exists while the
  VM runs. If the VM's network changes, pass another bind address:
  `./host/mt-capture 0.0.0.0 9999` and point `mt-inject --connect` at it.

## Known gaps / next steps

- The physical trackpad click (BTN_LEFT) is not streamed yet: MTTouch carries
  contacts, not the button. Add a CGEventTap on the host for left/right mouse
  button and a small button frame in the protocol.
- `size`/pressure scaling (`majorAxis / 10`) is a first guess; tune against how
  `touch.rs` reads contact size and whole-pad load.
- Y is flipped in the guest (Mac origin bottom-left, Linux MT top-left). If pan
  feels inverted, that is the knob.
