# yuka

A Wayland compositor and window manager built as an infinite canvas: windows are
quads you smoothly zoom, pan, and move, drawn through a single shader pass that
applies to every window.

Written in Rust. Rendering goes through raylib (GLES2 on EGL); the Wayland
protocol frontend is Smithay. On real hardware raylib owns the display and drives
KMS directly; for development it can instead run nested as a client of another
compositor.

See `CLAUDE.md` for the design and coding conventions, `lua/` for the behaviour
(bindings, gesture routing) and every tunable setting, and `devlog/` for the
running history.

## Requirements

- A Linux system with a DRM/KMS-capable GPU (a `/dev/dri/card0` node). This
  includes a virtio-gpu inside a VM, see "Developing from macOS in a VM" below.
- A Rust toolchain. The channel is pinned to `stable` by `rust-toolchain.toml`,
  so `rustup` installs the right one automatically.
- A C toolchain and CMake: the vendored raylib in `third_party/raylib` is built
  from source and linked statically.

## Dependencies

The build links libdrm, gbm, EGL, GLESv2, libinput, and xkbcommon; raylib is
compiled in. libseat is loaded at runtime with `dlopen` (through logind/seatd),
not linked.

On Debian/Ubuntu:

```sh
# build tools
sudo apt install build-essential pkg-config cmake

# libraries (build + runtime)
sudo apt install \
  libdrm-dev libgbm-dev libegl-dev libgles-dev \
  libinput-dev libxkbcommon-dev

# session control at runtime (pulls in libseat)
sudo apt install seatd

# extra, only for the nested/windowed build (raylib's GLFW Wayland backend)
sudo apt install libwayland-dev wayland-protocols

# the demo clients yuka spawns on startup, so the canvas has windows to play with
sudo apt install weston

# Rust, if you do not have it
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

You also need working Mesa drivers for your GPU (the GLES/EGL/gbm userspace),
which a normal desktop install already provides.

Then, to run on a bare TTY without root, enable seatd and put yourself in the
device groups (log out and back in for the group change to take):

```sh
sudo systemctl enable --now seatd
sudo usermod -aG video,input,render "$USER"
```

## Building

```sh
# DRM build (default): raylib owns the display and drives KMS.
cargo build

# Nested build: raylib is a Wayland client via GLFW, for development.
cargo build --features windowed
```

The `windowed` feature is the only difference between the two: same source, a
different raylib platform, with the DRM-only paths (session control, cursor
plane, libinput) staying out of the way.

## Running

### On real Linux hardware (KMS)

yuka wants its own VT. Switch to a spare one (Ctrl+Alt+F3), log in, and run it.
It takes session control through logind/seatd, so VT switching keeps working:

```sh
./run_tty.sh            # ~600 frames (about ten seconds), then exits
./run_tty.sh 1800       # longer, for actually interacting with it
```

`run_tty.sh` is deliberately safe on a fresh TTY: it stops itself after a frame
count (so a run that renders nothing still ends without a reboot) and writes its
log to a file (so output survives the TTY it started from). If it ever locks up,
Ctrl+Alt+F1 gets you back and `pkill -x yuka` stops it.

On startup yuka spawns a few weston demo clients (a terminal, an editor, a couple
of dmabuf toys) so the canvas has windows to pan, zoom, and drag around. Install
`weston` to get them; without it the canvas just comes up empty.

For an unbounded run with debug overlays on:

```sh
./run_debug.sh
```

### Working from a terminal inside yuka

To keep working from inside the canvas (an editor, a shell, a Claude Code session),
run your work in tmux on the first VT and attach to it from the weston-terminal that
yuka opens on startup.

1. On VT1, start or keep a tmux session going (`tmux new -s main`, or whatever
   is already running; `tmux ls` lists them).
2. Switch to a spare VT (Ctrl+Alt+F3), log in, and start yuka without a frame
   limit. `run_tty.sh` stops after a frame count, so use `run_debug.sh` or the
   binary directly:

   ```sh
   cd ~/repos/yuka && ./target/debug/yuka > /tmp/yuka.txt 2>&1 < /dev/null
   ```

   The `< /dev/null` keeps raylib off the tty keyboard, which otherwise can come
   back dead after yuka exits.
3. Click the weston-terminal window to focus it and attach:

   ```sh
   tmux attach -t main
   ```

   Both clients now share the session. Use `tmux attach -d -t main` to detach the
   VT1 client if the two sizes fight. The session survives yuka exiting, so
   Ctrl+Alt+F1 still has it.

There is no keybinding to launch a client yet. If the terminal is gone, start one
from another VT against yuka's socket. yuka prints it at startup
(`yuka: WAYLAND_DISPLAY=wayland-1`, in the log):

```sh
WAYLAND_DISPLAY=wayland-1 XDG_RUNTIME_DIR=/run/user/$(id -u) weston-terminal &
```

### Nested, for fast iteration

From inside an existing Wayland (or X11) session, run the windowed build. It
opens in a window, takes no session control, and a crash never takes the display
down with it:

```sh
cargo run --features windowed
```

### Environment variables

| Variable | Effect |
|----------|--------|
| `YUKA_MAX_FRAMES=<n>` | Stop after n frames. Unset means run forever. |
| `YUKA_SIZE=<W>x<H>` | DRM only: pick a specific connector mode, e.g. `1280x720`. Unset takes the whole screen (the connector's current mode), which on a virtual GPU can fall back to something tiny. |
| `YUKA_CARD=logind\|raylib` | Who opens the KMS device. Default `logind` (yuka opens it and hands raylib the fd); `raylib` lets raylib open the card itself. |
| `YUKA_NO_SEAT=1` | Do not take session control. Implied when there is no keyboard; also what non-interactive runs want. |
| `YUKA_SHOT[=<n>]` | Save a screenshot at frame n (a default frame if no number). |
| `YUKA_LOG=<filter>` | Log filter, e.g. `YUKA_LOG=debug` or `YUKA_LOG=smithay=trace`. Default `info`. |
| `YUKA_LOG_FILE=<path>` | Where `run_tty.sh` writes the log. |
| `YUKA_LUA=<dir>` | Script directory to run. Default `lua` in the working directory. |
| `YUKA_DEBUG_FPS`, `YUKA_DEBUG_INPUT`, `YUKA_DEBUG_LABELS`, `YUKA_DEBUG_PAD`, `YUKA_DEBUG_TREE` | Turn on individual debug overlays. |

### Settings and controls

The Rust side is the engine; what it does with your input is decided by Lua
scripts (LuaJIT) in `lua/`:

- `lua/init.lua` is the entry point: key bindings, where the wheel, finger scroll
  and pinch go (a window or the canvas), and the order everything runs in each
  frame. The engine functions and the per-frame snapshot are listed at the top.
- `lua/view.lua` moves the camera: zoom and pan, the spring that carries the view
  to a window, the 1:1 detent.
- `lua/gestures.lua` says what the three-finger swipe and tap and the Super double
  clicks mean.
- `lua/grab.lua` starts Super+drag moves and Super+right-drag resizes, and picks the
  corner a resize holds.
- `lua/windows.lua` decides where new windows go, focus (click to focus, new
  windows, activation requests, a focused window leaving the view), what maximize
  means, and the programs started with yuka.
- `lua/looks.lua` sets the background, fades windows that cover the focused one,
  and draws over the canvas through `yk.draw` (the fps counter lives here).
- `lua/config.lua` holds the settings the engine itself reads (trackpad, rendering,
  zoom limits), documented inline. Defaults live in `src/settings.rs`; a key left
  out falls back to them. Tuning for behaviour lives in a `tune` table at the top
  of the module that uses it.
- `lua/keys.lua` maps key names to evdev codes for chords like `"super+w"`.

The scripts are read at startup and re-read while running: save any file under
`lua/` and the change lands on the next frame, or press Super+R to reload on
demand. yuka will not start without scripts that load. Once running, a save
that fails to load leaves the previous scripts running and shows the error
across the top of the screen. Ctrl+Alt+Backspace (quit) and Ctrl+Alt+Fn
(switch VT) stay in the engine so a broken script can never lock you in.

The canvas is driven with the Super key and the trackpad. Broadly: hold Super and
two fingers pan and pinch the canvas, the wheel zooms, and a drag moves the window
under the pointer (Super+right-drag resizes it); three fingers pan, pinch, and
move between windows whatever the keyboard is doing; Super+Escape lets go of every
window; Super+W asks the focused window to close. All of that lives in `lua/`.

## Developing from macOS in a VM

macOS cannot run a Wayland compositor, so on a Mac you develop against a Linux
guest. On Apple Silicon the graphics path that gives the guest a real DRM/KMS
node plus accelerated GL/Vulkan is UTM's QEMU backend with a `virtio-gpu-gl`
device (Venus), running an arm64 Debian guest. Parallels and the Apple
Virtualization backend hide the graphics stack, so they are worse for this.

Setup, in short:

1. UTM (5.0+, for the Venus/accelerated GL backend), new Linux VM, arm64,
   QEMU backend. Set the display card to `virtio-gpu-gl-pci`.
2. Install arm64 Debian in the guest, then install the dependencies above. The
   guest's Mesa needs the Venus driver (Debian 13 / Mesa 25 has it). Confirm with
   `vulkaninfo --summary` (want a `Virtio-GPU Venus` device) and check
   `/dev/dri/card0` exists.

For the source, share this repo into the guest rather than cloning it, so you keep
editing on the Mac:

- In UTM, add a VirtFS/9p **Shared Directory** pointing at this repo and mount it
  in the guest (`mount -t 9p -o trans=virtio,version=9p2000.L share <path>`).
- The share is **read-only** to the guest, so build output cannot live in it. Set
  `CARGO_TARGET_DIR` to a local path in the guest (e.g.
  `~/.cache/yuka-target`) so builds are fast and never touch the share.

Then build and run in the guest over SSH. The compositor renders to the guest's
console (the UTM window): because there is an active session on that VT, libseat
hands it the card even when you launch over SSH. Keyboard input attaches when you
interact with the UTM window, not over SSH.

```sh
# from the Mac
ssh <guest> 'export CARGO_TARGET_DIR=$HOME/.cache/yuka-target; \
             cd ~/repos/yuka && cargo build'
ssh <guest> 'cd ~/repos/yuka && CARGO_TARGET_DIR=$HOME/.cache/yuka-target \
             ./run_tty.sh 600'
```

Two things that bite over a non-interactive SSH shell on Debian: `~/.bashrc`
returns early, so `cargo` is not on `PATH` (source `~/.cargo/env` or use the full
path), and any exports you rely on must be set on the command line rather than
assumed from the profile.

### Resolution and input in the VM

The virtio-gpu connector often defaults to a tiny mode. Since the DRM path takes
the connector's current mode, pass `YUKA_SIZE=WxH` to pick one it offers (see
`cat /sys/class/drm/card0-Virtual-1/modes`), e.g. `YUKA_SIZE=1440x900`.

Input has a real limitation: UTM/QEMU forwards only pointer motion, clicks, and
scroll from the host. The Mac trackpad appears in the guest as a plain mouse/tablet
(`/proc/bus/input/devices` shows only QEMU USB Tablet/Mouse/Keyboard), so the
custom multitouch gesture engine (`touch.rs`, `trackpad_mode = custom`) has no
multitouch device to read. You can drive the compositor with pointer + scroll +
keyboard, but native trackpad gestures need real hardware, or the mt-bridge below.

### Trackpad gestures in the VM (mt-bridge)

`tools/mt-bridge/` carries the real Mac trackpad into the guest: a host program
reads raw multitouch (and clicks) and streams them to a guest program that
synthesizes a `uinput` touchpad `touch.rs` reads as if it were a real pad. Full
detail (protocol, permissions, focus rules) is in `tools/mt-bridge/README.md`;
the short version:

```sh
# build once
tools/mt-bridge/host/build.sh                 # on the Mac  -> ./mt-capture
ssh <guest> 'bash ~/repos/yuka/tools/mt-bridge/guest/build.sh'   # -> ~/mt-inject

# each session, in order
ssh <guest> '~/mt-inject &'                   # 1. create the uinput touchpad
tools/mt-bridge/host/mt-capture               # 2. stream the trackpad (real terminal)
ssh <guest> 'cd ~/repos/yuka && ~/run_yuka_tty.sh'   # 3. yuka finds the pad at start
```

`~/run_yuka_tty.sh` is a machine-local copy of `run_yuka_tty.example.sh` in this
repo: it sets the VM-specific knobs (a local `CARGO_TARGET_DIR` for the read-only
9p share, the checkout path, the connector mode) and adds the console-safety bits
the in-tree `run_tty.sh` leaves out. Copy the example somewhere writable, edit the
three knobs at the top, and run that copy. The example is not machine-specific
config in version control on purpose; the copy you run is.

Then, with the mt-capture terminal frontmost, touch the trackpad and watch the
(unfocused) UTM window: two fingers pan/pinch, one finger points, a click clicks.
Three things worth knowing: run mt-capture in a real terminal (Terminal.app/iTerm)
with Input Monitoring granted, since multitouch is delivered only to the frontmost
app and clicks need the permission; the guest needs `/dev/uinput` writable (the
one-time udev rule in `tools/mt-bridge/README.md`); and disable macOS's own 3/4-
finger gestures in System Settings > Trackpad so they do not fire alongside
yuka's.

One console gotcha specific to KMS: launch the binary with stdin off the console
(`yuka < /dev/null`) so raylib does not also grab the tty keyboard on top of
logind's session control; otherwise the console keyboard can be left in raw mode
on exit. Recover with `sudo kbd_mode -u -f -C /dev/tty1`.
