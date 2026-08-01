# Weston demo clients as an om_wm test checklist

Weston ships small demo clients that each exercise a slice of the Wayland
protocol. They are a handy black-box test suite for om_wm. Spawn them by name
(om_wm sets `WAYLAND_DISPLAY`); the launch set lives in `src/main.rs`.

om_wm currently implements: `wl_compositor` (+ subsurfaces), `wl_shm`,
`xdg_shell`, `wp_viewporter`, `zwp_linux_dmabuf_v1` (v4 feedback), and `wl_seat`
(pointer + keyboard). Anything needing other protocols is marked N/A below.

Legend: `[x]` verified on om_wm · `[ ]` not yet tested · `N/A` needs an
unimplemented protocol/device.

## Interaction (pointer / keyboard)

- [x] **weston-terminal** — real shell. Keyboard entry + pointer selection.
  _Verified: typing forwards correctly (fixed the evdev+8 xkb offset)._
- [x] **weston-eventdemo** — no visible UI; prints received input events to
  stdout. _Verified: pointer motion arrives with correct surface-local coords._
- [x] **weston-smoke** — smoke billows and follows the pointer (shm, animated).
  _Verified: animates and tracks the pointer._
- [ ] **weston-clickdot** — click → draws a dot at the click point. Pointer-click
  landing test.
- [ ] **weston-editor** — text editor. Click to place caret, type text. (Uses
  text-input for IME; basic keys via wl_keyboard should still type.)
- [ ] **weston-resizor** — animates its own size; exercises xdg configure/resize.
- [ ] **weston-stacking** — multiple toplevels / z-order.

## Rendering / animation

- [x] **weston-simple-egl** — spinning RGB triangle via EGL → dmabuf (zero-copy).
  _Verified: animates, zero-copy import + cache._
- [ ] **weston-flower** — animated spinning flower (shm). _Rendered but animation
  did not advance in one run; smoke works, so frame-callback delivery is fine —
  likely a demo quirk. Re-check._
- [ ] **weston-simple-shm** — animated color gradient (shm).
- [ ] **weston-simple-damage** — shm with explicit damage regions.
- [ ] **weston-scaler** — wp_viewporter crop/scale (we support viewporter).
- [ ] **weston-subsurfaces** — nested subsurface compositing (our surface tree).
- [ ] **weston-image** — image viewer.
- [ ] **weston-cliptest** — renderer clipping/tessellation dev test.
- [ ] **weston-fullscreen** — fullscreen-shell test (we don't special-case
  fullscreen, so it shows as a normal window).
- [ ] **weston-transformed** — output transform/rotation.

## dmabuf variants

- [ ] **weston-simple-dmabuf-egl** — explicit linux-dmabuf triangle.
- [ ] **weston-simple-dmabuf-feedback** — exercises dmabuf v4 feedback.
- `N/A` **weston-simple-dmabuf-v4l** — dmabuf from a V4L camera (needs a webcam).

## N/A — needs protocols/devices we haven't implemented

- `N/A` **weston-dnd** — drag-and-drop; needs data-device.
- `N/A` **weston-constraints** — pointer lock/confine; needs pointer-constraints
  + relative-pointer.
- `N/A` **weston-presentation-shm** — presentation-time feedback.
- `N/A` **weston-simple-touch** / **weston-calibrator** /
  **weston-touch-calibrator** — touch input (not forwarded).
- `N/A` **weston-tablet** — graphics-tablet protocol.
- `N/A` **weston-multi-resource** — resource stress test (runs, uninteresting).
- `N/A` **weston-screenshooter** / **weston-debug** /
  **weston-content_protection** — weston-private protocols.
