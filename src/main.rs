//
// om_wm
//
// Wayland compositor + window manager on an infinite canvas, rendered with
// raylib on DRM/KMS. Current milestone: import client buffers (shm copy and
// zero-copy dmabuf) into GL textures and draw them as shader-processed quads.
// Infinite canvas and input come next.
//

mod camera;
mod cursor;
mod egl;
mod input;
mod ray;
mod render;
mod settings;
mod seat;
mod touch;
mod wl;

use std::ffi::c_int;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use smithay::backend::input::{Axis, AxisSource, ButtonState, KeyState};
use smithay::input::keyboard::{FilterResult, Keycode};
use smithay::input::pointer::CursorImageStatus;
use smithay::input::pointer::{AxisFrame, ButtonEvent, MotionEvent};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::backend::ObjectId;
use smithay::reexports::wayland_server::Resource;
use smithay::utils::{Logical, Point, SERIAL_COUNTER};

use render::Windows;
use wl::state::State;

//
// Constants
//

// Default frame for OM_WM_SHOT; OM_WM_SHOT=<n> picks another, which matters when
// what you want to see only exists a few seconds in.
const SHOT_FRAME: u32 = 200;
const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;
// How long to keep a released resize stretched while waiting for the client to commit
// the size we asked for. A client that ignores the ask gets snapped back after this, so
// a stretched window is never left lying about its size.
const RESIZE_SETTLE_FRAMES: u32 = 90;
// How often to stat the settings file. Once a second is instant to a human hitting
// save, and 60 stats a minute is nothing next to a frame.
const SETTINGS_POLL_FRAMES: u32 = 60;
// How long to sleep per iteration while another VT owns the display.
const VT_IDLE_MS: u64 = 30;
// Size of the window in the nested build. On DRM we take the whole screen instead.
const WINDOWED_W: i32 = 1280;
const WINDOWED_H: i32 = 800;

//
// State
//

static RUNNING: AtomicBool = AtomicBool::new(true);

// Which of the two ways of working is in force. They differ in who owns the pointer,
// which is the one decision everything else follows from.
//
// Desk: the canvas owns it. Pan, zoom and gestures are live, a drag moves a window and a
// right drag resizes it, no modifier needed, and clients receive nothing at all, not
// pointer events and not the keyboard. Windows are objects on a desk.
//
// Work: the client owns it. The camera is frozen, so a scroll always goes to the window
// under the pointer, and clicks, drags, selections and typing all land in it. Windows
// move and resize the way their own toolkit expects, through xdg_toplevel move and resize
// requests, with Super+drag still there as the compositor-side way.
//
// Modal design needs to be visible, hence the badge on screen: a mode you cannot see is a
// mode that will surprise you.
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Desk,
    Work,
}

// A window being dragged: which one, the offset from the cursor to its origin, and whether
// the client started it.
//
// Who started it decides what happens to the button release. Ours consumes it, since the
// client never saw the press that began the drag and a release without a press is a click
// out of nowhere. A client's own titlebar drag is the opposite: it pressed, it asked us to
// move it through xdg_toplevel.move, and it is waiting for the release to end its own drag
// state. Swallowing that left it stuck in the drag, with hover and cursor behaving oddly
// until the next click gave it a fresh press and release.
struct Drag {
    surface: WlSurface,
    off_x: f32,
    off_y: f32,
    from_client: bool,
    // Where the cursor and the window were when the drag began, for the debug trace: the
    // question is whether the window travels exactly as far as the cursor does.
    start_cursor: (f32, f32),
    start_pos: (f32, f32),
}

// An in-progress Super+right-drag resize.
//
// Two things happen at once, because a resize on Wayland cannot be immediate: the
// configure goes out, the client re-renders, commits, we import and upload, we draw.
// That round trip is why waiting for the client made the window lerp behind the cursor.
//
// So the geometry is ours and the pixels are the client's. Every frame the quad is
// scaled to exactly where the cursor is, which costs nothing and cannot lag. In parallel
// the client is told the same size as fast as it can answer, so its content keeps
// reflowing at whatever rate it manages: text rewraps, a page relayouts, and the
// stretch shrinks toward 1.0 as its buffer catches up with the quad it is being drawn
// into. Neither half waits for the other.
//
// Sizes are computed from where the drag started rather than accumulated per frame, so
// nothing drifts, and the scale is recomputed against whatever the client has actually
// committed, so a commit mid-drag changes the pixels without moving the corner.
struct Resize {
    surface: WlSurface,
    // Canvas point where the drag started, and the window's real origin and size then.
    grab_x: f32,
    grab_y: f32,
    from_x: f32,
    from_y: f32,
    from_w: f32,
    from_h: f32,
    // Which edges are moving, per axis: 1 for the far edge (right, bottom), -1 for the
    // near one, 0 for an axis that is not being resized. A near edge moves the window's
    // origin as well as its size, which is why the origin is remembered above. Our own
    // Super+drag always uses the far corner; a client dragging its own edge can ask for
    // any of them.
    edge_x: f32,
    edge_y: f32,
    // Which button has to stay down for this drag to continue. Ours is the right one,
    // since Super+right-drag is how the compositor resizes; a client dragging its own edge
    // is holding the left one, and watching the wrong button ended those resizes on the
    // frame after they started.
    right_button: bool,
    // The last size we asked for, what the client was drawing when we asked, and how many
    // frames ago. Asking again is gated on the client having answered, because a
    // configure it has not caught up with yet is a re-render it will throw away.
    asked: (f32, f32),
    seen: (f32, f32),
    waited: u32,
    // The smallest and largest the client has actually managed during this drag, and how
    // many commits it has made since either improved. Most clients declare no limits at
    // all (weston's toolkit reports none) and simply refuse to go further, so the limits
    // have to be learned. Stretching past a refusal is a lie the release has to undo.
    //
    // Learned from what the client manages, not by pairing asks with answers: with asks
    // going out every frame a commit answers something several asks old, and reading that
    // as a refusal produced contradictory limits, one of which crossed the other and
    // panicked a clamp. Monotone behaviour is the honest signal, and it does not care how
    // far behind the client is.
    least_w: f32,
    least_h: f32,
    most_w: f32,
    most_h: f32,
    stuck_small: u32,
    stuck_large: u32,
}

// How many commits without progress mean the client has stopped moving in that direction.
// A couple would misread a client that is merely a frame or two behind.
const RESIZE_STUCK_COMMITS: u32 = 5;
// And how much progress counts as progress, since a terminal answers in whole character
// cells and a pixel of rounding is not movement.
const RESIZE_PROGRESS_PX: f32 = 4.0;

// A resize that has been released and asked for, waiting for the client to answer so the
// stretch can come off.
struct ResizeSettle {
    surface: WlSurface,
    // The size the client was drawing when we asked. Any change from this means it has
    // answered, whatever it decided.
    was: (f32, f32),
    frames: u32,
}

extern "C" fn on_signal(_sig: c_int) {
    RUNNING.store(false, Ordering::Relaxed);
}

//
// Client spawn
//

fn spawn_client(socket_name: &str, cmd: &str) -> Option<Child> {
    match Command::new(cmd)
        .env("WAYLAND_DISPLAY", socket_name)
        .env("QT_QPA_PLATFORM", "wayland")
        .env("GDK_BACKEND", "wayland")
        .env("XDG_SESSION_TYPE", "wayland")
        .spawn()
    {
        Ok(child) => {
            println!("om_wm: spawned '{cmd}' (pid {})", child.id());
            Some(child)
        }
        Err(e) => {
            eprintln!("om_wm: failed to spawn '{cmd}': {e}");
            None
        }
    }
}

// Where the press with this serial landed and on which frame, or the current pointer if we
// no longer have it. A client quotes the serial of the press that started a move or resize,
// and that press is the only honest anchor for the drag. The frame comes back with it so the
// debug trace can say how long the client took to ask.
fn press_at(
    log: &[(u32, f32, f32, u32)],
    serial: u32,
    now_x: f32,
    now_y: f32,
    now_frame: u32,
) -> (f32, f32, u32) {
    log.iter()
        .rev()
        .find(|(s, _, _, _)| *s == serial)
        .map(|(_, x, y, f)| (*x, *y, *f))
        .unwrap_or((now_x, now_y, now_frame))
}

//
// Pointer routing
//
// Turns the cursor position + click edges into Wayland pointer events for the
// focused window. Canvas coordinates are the Wayland global compositor space, so
// a window's surface origin is its canvas position and the pointer location is
// the cursor's canvas position; Smithay derives surface-local coords + focus.
fn route_input(
    state: &mut State,
    windows: &mut Windows,
    cam3d: ray::Camera3D,
    cursor_pos: Option<(i32, i32)>,
    focused: &mut Option<WlSurface>,
    hovered: &mut Option<WlSurface>,
    grabbed: &mut Option<WlSurface>,
    // What the client was last told about the pointer: which surface, and where in
    // that surface's own coordinates.
    last_motion: &mut Option<(WlSurface, f32, f32)>,
    held: bool,
    kb: Option<&input::Input>,
    debug: bool,
    super_down: bool,
    // True while we are moving or resizing a window because the client asked us to. The
    // compositor owns the pointer for the duration of an xdg_toplevel move or resize, and a
    // client does not expect motion inside itself while it believes it is being dragged.
    // Sending it anyway is what made Chromium jump: it reconciles our motion against its own
    // drag when the drag ends. Super+drag never showed it, because Super suppresses client
    // pointer events entirely, which is the same thing by a different route.
    //
    // Buttons still go through, since the release is what ends the client's own drag.
    client_grab: bool,
    // Where each button press landed, against the serial sent with it. A client asking to be
    // moved or resized quotes that serial, and anchoring the drag to the press rather than to
    // the pointer's current position is the difference between dragging from the point you
    // grabbed and dragging from wherever the pointer reached while the client was deciding.
    press_log: &mut Vec<(u32, f32, f32, u32)>,
    ptr: &input::Pointer,
    pressed: bool,
    released: bool,
    time_ms: u32,
    frame: u32,
) {
    let Some((cxp, cyp)) = cursor_pos else { return };
    let (ccx, ccy) = camera::screen_to_plane(cam3d, cxp as f32, cyp as f32, 0.0)
        .unwrap_or((0.0, 0.0));
    let loc = Point::<f64, Logical>::from((ccx as f64, ccy as f64));
    let pointer = state.pointer.clone();
    let keyboard = state.keyboard.clone();

    // Super+Escape drops keyboard focus.
    let super_escape = kb
        .map(|kb| input::super_down(kb) && input::down(kb, input::KEY_ESC))
        .unwrap_or(false);
    if super_escape && focused.is_some() {
        keyboard.set_focus(state, None, SERIAL_COUNTER.next_serial());
        *focused = None;
    }

    // Super claims the pointer for the canvas: no hover, no buttons, no scroll for
    // clients while it is held, so zooming and dragging windows never make the app
    // under the cursor react. Pointer focus is withdrawn on the way in, so nothing
    // is left believing the cursor is still inside it.
    if super_down {
        if hovered.is_some() {
            let serial = SERIAL_COUNTER.next_serial();
            pointer.motion(state, None, &MotionEvent { location: loc, serial, time: time_ms });
            pointer.frame(state);
            *hovered = None;
            *last_motion = None;
        }
        return;
    }

    // Pointer input follows the pointer; keyboard input follows focus. That split
    // is what Wayland clients are built for, and menus depend on it: a popup has
    // to receive enter and motion to know which item is under the cursor, and it
    // never has keyboard focus while you are reaching for an item. Routing motion
    // to the focused window instead left menus unable to see the pointer at all,
    // so clicking an item did nothing.
    // One hit test for everything on the canvas: windows, their popups and their
    // subsurfaces, all anchored in front of their parents.
    let mut under = render::window_at(windows, cam3d, cxp as f32, cyp as f32);

    // Implicit pointer grab: from a press until the last button comes up, every
    // event belongs to the surface the press landed on, wherever the pointer goes.
    // Without it a drag that leaves the surface it started on (text selection, a
    // scrollbar, dragging a menu item) silently changes recipient halfway.
    //
    // It has to outlive the release it ends on. Clearing it as soon as the button
    // level drops meant the release itself was routed by hover instead of by the
    // grab, so a click whose pointer had wandered even slightly delivered its press
    // to the item and its release to something else, and nothing activated.
    if let Some(surf) = grabbed.clone() {
        match render::surface_origin(windows, &surf) {
            Some((ox, oy)) => under = Some((surf, ox, oy)),
            // The grabbed surface went away; let the grab lapse.
            None => *grabbed = None,
        }
    }
    // Descend into subsurfaces: the pointer belongs to the surface actually under
    // it, which is not always the root one the hit test found.
    let under = under.map(|(surf, ox, oy)| {
        match wl::state::surface_under(&surf, (ccx - ox) as f32, (ccy - oy) as f32) {
            Some((child, dx, dy)) if child != surf => (child, ox + dx, oy + dy),
            _ => (surf, ox, oy),
        }
    });
    let target = under.as_ref().map(|(surf, _, _)| surf.clone());
    let entered = target != *hovered;
    *hovered = target.clone();

    // Send motion whenever the client's view of the pointer changes: which surface
    // it is over, or where it is inside that surface. The mouse moving is only one
    // way for that to happen, and keying off it alone was a bug you could feel:
    // panning, zooming, or dragging a window under a still cursor moves the pointer
    // in surface coordinates, and a client that is never told keeps acting on where
    // the pointer used to be, so its clicks land in the wrong place or nowhere.
    //
    // Still not every frame: when nothing has changed we stay quiet, because a
    // client reading repeated motion treats it as continuous movement (weston-smoke
    // never stops smoking).
    let now_at = under
        .as_ref()
        .map(|(surf, ox, oy)| (surf.clone(), ccx - ox, ccy - oy));
    let changed = match (&*last_motion, &now_at) {
        (Some((was, wx, wy)), Some((is, ix, iy))) => {
            was != is || (wx - ix).abs() > 0.01 || (wy - iy).abs() > 0.01
        }
        (None, None) => false,
        _ => true,
    };
    *last_motion = now_at;
    if (changed || entered) && !client_grab {
        let focus = under
            .as_ref()
            .map(|(surf, ox, oy)| (surf.clone(), Point::<f64, Logical>::from((*ox as f64, *oy as f64))));
        let serial = SERIAL_COUNTER.next_serial();
        pointer.motion(state, focus, &MotionEvent { location: loc, serial, time: time_ms });
        pointer.frame(state);
    }

    // Buttons. All three go to whatever the implicit grab points at, so a press and
    // its release can never land on different surfaces, and a release is never
    // dropped for want of something under the pointer. Dropping one is worse than
    // misplacing it: a client left believing a button is still held goes into a drag
    // or, for Chromium, middle-click autoscroll, and ignores everything after. Six
    // presses to one release is what that looked like on the wire.
    //
    // Left is also what moves keyboard focus and raises a window; the others never
    // do. Clicking empty canvas drops focus.
    if pressed && debug {
        match under.as_ref() {
            Some((_, ox, oy)) => println!(
                "om_wm: press -> surface local {:.0},{:.0} (origin {ox:.0},{oy:.0})",
                ccx - ox,
                ccy - oy
            ),
            None => println!("om_wm: press -> nothing under the pointer"),
        }
    }

    let edges = [
        (BTN_LEFT, pressed, released),
        (BTN_RIGHT, ptr.right_pressed, ptr.right_released),
        (BTN_MIDDLE, ptr.middle_pressed, ptr.middle_released),
    ];

    // Any press starts the grab, so the rest of that click belongs to this surface.
    if edges.iter().any(|(_, down, _)| *down) {
        if let Some((surf, _, _)) = under.as_ref() {
            *grabbed = Some(surf.clone());
        }
    }

    if pressed {
        match under.as_ref() {
            Some((surf, _, _)) => {
                // Pointer input goes to the exact surface under the cursor, but
                // keyboard focus follows the *toplevel*, never a popup or a
                // subsurface, and only when it actually changes.
                //
                // Both halves matter. Smithay implements a focus change as leave then
                // enter, and a client reads leave on its toplevel as "you lost
                // focus": Chromium destroys its open menus the instant it arrives. So
                // handing focus to the popup killed the menu before the click landed,
                // and re-sending focus a window already has would do the same for no
                // reason at all.
                let window = wl::state::window_root(state, surf);
                render::front(windows, &window);
                if focused.as_ref() != Some(&window) {
                    *focused = Some(window.clone());
                    keyboard.set_focus(state, Some(window), SERIAL_COUNTER.next_serial());
                }
            }
            None => {
                if focused.is_some() {
                    keyboard.set_focus(state, None, SERIAL_COUNTER.next_serial());
                    *focused = None;
                }
            }
        }
    }

    let mut sent = false;
    for (button, down, up) in edges {
        if !down && !up {
            continue;
        }
        if under.is_none() {
            if debug {
                println!("om_wm: button {button:#x} had nothing under the pointer");
            }
            continue;
        }
        if down {
            let serial = SERIAL_COUNTER.next_serial();
            // Remember where this press was, so a move or resize request quoting its serial
            // can be anchored to it. A handful is plenty: a client asks within a frame or
            // two, and an older entry is of no use to anyone.
            press_log.push((u32::from(serial), cxp as f32, cyp as f32, frame));
            if press_log.len() > 16 {
                press_log.remove(0);
            }
            pointer.button(state, &ButtonEvent { serial, time: time_ms, button, state: ButtonState::Pressed });
            sent = true;
        }
        if up {
            let serial = SERIAL_COUNTER.next_serial();
            pointer.button(state, &ButtonEvent { serial, time: time_ms, button, state: ButtonState::Released });
            sent = true;
        }
    }
    if sent {
        pointer.frame(state);
    }

    // Now the grab may lapse, once the release it was holding for has gone out.
    if !held {
        *grabbed = None;
    }
}

// Tell the focused window that every key we think is held is up. Called before
// handing the keyboard to another VT: the releases will land there instead of
// here, and a client left believing ctrl or alt is down misreads everything the
// user types next.
fn release_held_keys(
    state: &mut State,
    kb: &input::Input,
    focused: &Option<WlSurface>,
    time_ms: u32,
) {
    if focused.is_none() {
        return;
    }
    let keyboard = state.keyboard.clone();
    let held: Vec<u16> = input::keys(kb)
        .iter()
        .enumerate()
        .filter(|(_, down)| **down)
        .map(|(code, _)| code as u16)
        .collect();
    for code in held {
        let serial = SERIAL_COUNTER.next_serial();
        keyboard.input::<(), _>(
            state,
            Keycode::new(code as u32 + 8),
            KeyState::Released,
            serial,
            time_ms,
            |_, _, _| FilterResult::Forward,
        );
    }
}

// Forward scrolling to whatever the pointer is over: over a window the wheel
// belongs to the client, over empty canvas it zooms the canvas. Wheels send discrete v120 steps plus the
// pixel value clients expect, finger scroll sends pixels. Our sign convention is
// positive up and right, Wayland's is positive down and right, so the vertical
// axes flip back here.
fn forward_scroll(
    state: &mut State,
    ptr: &input::Pointer,
    time_ms: u32,
    set: &settings::Settings,
) {
    let pointer = state.pointer.clone();
    let scroll_sign = settings::client_scroll_sign(set);
    let hscroll_sign = settings::client_hscroll_sign(set);
    if ptr.wheel != 0.0 || ptr.hwheel != 0.0 {
        let mut frame = AxisFrame::new(time_ms).source(AxisSource::Wheel);
        if ptr.wheel != 0.0 {
            let v = ptr.wheel * scroll_sign;
            frame = frame
                .v120(Axis::Vertical, (v * 120.0) as i32)
                .value(Axis::Vertical, (v * set.wheel_step_px) as f64);
        }
        if ptr.hwheel != 0.0 {
            let h = ptr.hwheel * hscroll_sign;
            frame = frame
                .v120(Axis::Horizontal, (h * 120.0) as i32)
                .value(Axis::Horizontal, (h * set.wheel_step_px) as f64);
        }
        pointer.axis(state, frame);
        pointer.frame(state);
    }
    if ptr.scroll_x != 0.0 || ptr.scroll_y != 0.0 {
        // Finger scroll into a window gets its own sensitivity: the same gesture serves
        // the canvas, where one to one is right, and a page, which usually wants more.
        let sens = set.window_scroll_sens;
        let mut frame = AxisFrame::new(time_ms).source(AxisSource::Finger);
        if ptr.scroll_y != 0.0 {
            frame = frame.value(Axis::Vertical, (ptr.scroll_y * scroll_sign * sens) as f64);
        }
        if ptr.scroll_x != 0.0 {
            frame = frame.value(Axis::Horizontal, (ptr.scroll_x * hscroll_sign * sens) as f64);
        }
        pointer.axis(state, frame);
        pointer.frame(state);
    }
}

// Forward keyboard press/release edges to the focused window. Super+Escape is a
// compositor shortcut (unfocus, handled in route_input) so it is not forwarded.
// Every key edge goes to Smithay's keyboard, focus or no focus.
//
// It tracks the xkb state, and that state has to see the same keys the keyboard did or it
// drifts. Skipping this while nothing was focused meant a key pressed with a window focused
// and released after focus went away delivered its press and dropped its release, so xkb
// went on believing the key was held. A stuck Ctrl turns typing "a" into select-all, which
// is how this was found. Delivery is Smithay's business: with no focus it updates state and
// sends nothing, which is exactly what desk mode wants.
fn forward_keys(state: &mut State, kb: &input::Input, time_ms: u32) {
    let keyboard = state.keyboard.clone();
    for &(code, pressed) in input::events(kb) {
        let chord = input::super_down(kb)
            && (code == input::KEY_ESC || code == input::KEY_0);
        if chord {
            continue;
        }
        let key_state = if pressed {
            KeyState::Pressed
        } else {
            KeyState::Released
        };
        let serial = SERIAL_COUNTER.next_serial();
        // Smithay expects the xkb keycode (evdev + 8).
        keyboard.input::<(), _>(
            state,
            Keycode::new(code as u32 + 8),
            key_state,
            serial,
            time_ms,
            |_, _, _| FilterResult::Forward,
        );
    }
}

//
// Entry
//

fn main() {
    // Smithay and wayland-server report everything through tracing, including
    // client protocol errors, and without a subscriber all of it is dropped. That
    // is why a client dying used to leave no trace at all. OM_WM_LOG overrides the
    // filter (e.g. OM_WM_LOG=debug, or OM_WM_LOG=smithay=trace).
    let filter = std::env::var("OM_WM_LOG").unwrap_or_else(|_| "info".to_string());
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
        .with_writer(std::io::stderr)
        .init();

    // The feel of the thing lives in a file, not in the binary: sensitivities, gesture
    // thresholds, camera rates. Re-read while running (see SETTINGS_POLL_FRAMES), so
    // tuning is a save from another tty rather than a rebuild.
    let conf_path =
        std::env::var("OM_WM_CONF").unwrap_or_else(|_| settings::DEFAULT_PATH.to_string());
    let mut set = settings::init(&conf_path);
    let mut conf_watch = settings::watch_new(&conf_path);

    // Nested build: om_wm is a client of another compositor, which owns the display
    // and the input devices. Everything that reaches for hardware stays out of it, so
    // there is no session to control, no DRM master to hold, no evdev to read, and
    // nothing that can lock the machine. Same source, one flag apart.
    let windowed = cfg!(feature = "windowed");

    // Session control. With it, logind owns our VT (K_OFF, KD_GRAPHICS) so the
    // switch chords are ours to implement and input needs no grab.
    //
    // Taking control makes us the only thing that can switch VTs, so we refuse it
    // when the machine has no keyboard to switch with: silencing the console then
    // would leave no way off this VT at all. OM_WM_NO_SEAT opts out by hand, which
    // is also what non-interactive runs want.
    let no_seat = std::env::var("OM_WM_NO_SEAT").is_ok();
    let mut seat = if windowed {
        println!("om_wm: nested in another compositor: no session control, no vt switching");
        None
    } else if input::any_keyboard_present() && !no_seat {
        seat::init()
    } else {
        if !input::any_keyboard_present() {
            eprintln!(
                "om_wm: no keyboard on this machine, so not taking session control: \
                 the console keeps its own ctrl+alt+Fn"
            );
        }
        None
    };

    // The card, before the window: logind hands out KMS devices, and raylib takes the
    // fd from us rather than opening one itself. Without the raylib patch there is
    // nothing to hand it to, so raylib opens the card and we adopt its fd afterwards.
    // OM_WM_CARD picks the opener by hand: logind (the default) or raylib, which is
    // also what a raylib without the patch gets. It keeps that older path tested, and
    // is the first thing to try if the handoff ever misbehaves.
    let mut handed_card = false;
    let handoff = match std::env::var("OM_WM_CARD").unwrap_or_default().as_str() {
        "" | "logind" => ray::can_set_drm_fd(),
        "raylib" => false,
        other => {
            eprintln!("om_wm: OM_WM_CARD={other} is neither logind nor raylib, using logind");
            ray::can_set_drm_fd()
        }
    };
    if let Some(s) = seat.as_mut() {
        if handoff {
            if let Some(fd) = seat::open_card(s) {
                ray::set_drm_fd(fd);
                handed_card = true;
            }
        }
    }

    if windowed {
        ray::disable_libdecor();
        ray::set_config_flags(ray::FLAG_WINDOW_UNDECORATED | ray::FLAG_VSYNC_HINT);
        ray::init_window(WINDOWED_W, WINDOWED_H, "om_wm (nested)");
    } else {
        ray::init_window(0, 0, "om_wm");
    }
    // No SetTargetFPS: the DRM page flip in EndDrawing already vsyncs to the
    // display. A second 60 Hz cap would beat against it and cause stutter.

    // The fallback path: the card is raylib's, so find its fd for the master handover
    // on a VT switch.
    if let Some(s) = seat.as_mut() {
        if !handed_card {
            seat::adopt_card(s);
        }
    }

    // Bail out before touching input if we have no display. Running on regardless is
    // the worst outcome available: no picture, while we hold the keyboard and, without
    // session control, leave no obvious way to switch away.
    // A card from logind needs no contention check: it is handed to one session at a
    // time, so a second om_wm is refused at open and ends up on the path below.
    let logind_card = seat.as_ref().map(seat::card_from_logind).unwrap_or(false);
    let display_ours = logind_card || seat::drm_is_ours();
    if ray::screen_width() <= 0 || ray::screen_height() <= 0 || !display_ours {
        eprintln!(
            "om_wm: no usable display ({}x{}), exiting before taking any input.",
            ray::screen_width(),
            ray::screen_height()
        );
        ray::close_window();
        // Releasing the session puts the VT back to text and a working keyboard.
        if let Some(s) = seat.as_mut() {
            seat::shutdown(s);
        }
        return;
    }

    // Install after InitWindow so nothing in EGL/GBM setup resets the handlers.
    let handler = on_signal as extern "C" fn(c_int) as usize;
    unsafe {
        libc::signal(libc::SIGINT, handler);
        libc::signal(libc::SIGTERM, handler);
    }

    // Escape must reach clients, not quit us; unfocus is Super+Escape.
    ray::set_exit_key(0);

    // Push the 3D far plane out past the camera's zoomed-out height so windows
    // on the z=0 plane are never clipped (raylib defaults to 0.01 .. 1000).
    ray::set_clip_planes(1.0, 200_000.0);

    println!(
        "om_wm: screen {}x{}  egl_display={:p}  egl_context={:p}",
        ray::screen_width(),
        ray::screen_height(),
        ray::egl_current_display(),
        ray::egl_current_context(),
    );

    // A label under every window saying what it is made of and how it is sampled.
    // OM_WM_DEBUG_LABELS=1, or Super+I at any time.
    let mut debug_labels = std::env::var("OM_WM_DEBUG_LABELS").is_ok();
    // The trackpad instrument in the corner: fingers, button regions, live gesture
    // state. OM_WM_DEBUG_PAD=1, or Super+P at any time.
    let mut debug_pad = std::env::var("OM_WM_DEBUG_PAD").is_ok();
    // Frame rate, this frame and the worst of the last second. OM_WM_DEBUG_FPS=1, or
    // Super+F at any time.
    let mut debug_fps = std::env::var("OM_WM_DEBUG_FPS").is_ok();

    let egl = egl::init().expect("egl init");
    // How much anisotropic filtering the driver offers, asked once. It only matters
    // for minified windows, which is also the only time we build the mip chain it
    // needs, and the perspective tilt is what makes it worth having at all.
    let anisotropy = egl::max_anisotropy();
    let dmabuf_formats = egl.query_formats();
    let render_node_dev = egl.render_node_dev();
    println!(
        "om_wm: egl reports {} dmabuf format/modifier pairs, anisotropy {anisotropy:.0}x",
        dmabuf_formats.len()
    );

    let shader = ray::load_shader("shaders/window.vert", "shaders/window.frag");
    let alpha_loc = ray::shader_location(shader, "alphaBlend");
    let swizzle_loc = ray::shader_location(shader, "swizzleBgra");

    let (mut server, mut state) =
        wl::state::init(
            dmabuf_formats,
            render_node_dev,
            ray::screen_width(),
            ray::screen_height(),
        )
        .expect("wayland init");
    println!("om_wm: WAYLAND_DISPLAY={}", server.socket_name);

    let mut windows = render::windows_new();
    let mut dmabuf_cache = render::dmabuf_cache_new();
    let mut cam = camera::camera_new(&set);
    // The hardware cursor plane is a DRM thing; nested, the host draws the pointer.
    let mut cursor = if windowed {
        None
    } else {
        cursor::init(ray::screen_width(), ray::screen_height())
    };


    // libinput drives keyboards and pointers. Without a session it grabs them as
    // it opens them, since the console would otherwise read everything we type,
    // and that costs us VT switching: ctrl+alt+backspace is then the way out.
    // libinput would read the real devices even nested, so the host and om_wm would
    // both react to every key. Host input comes through raylib instead.
    let mut inp = if windowed {
        Some(input::init_host(&set))
    } else {
        input::init(seat.is_none(), &set)
    };

    // Nothing to drive us: no device opened, or no libinput at all. Carrying on from
    // here is how you end up rebooting the machine, because with session control
    // logind has already silenced the console, so neither our chords nor the kernel's
    // work and there is no way left to quit. Session control is released on the way
    // out, which restores the VT.
    let opened = inp.as_ref().map(input::devices).unwrap_or(0);
    if opened == 0 && !windowed {
        eprintln!(
            "om_wm: libinput opened no input devices, so nothing could quit or switch \
             away from om_wm. Exiting instead of locking the seat. This is usually \
             permissions on /dev/input: try 'sg input -c ./target/debug/om_wm'."
        );
        if let Some(s) = seat.as_mut() {
            seat::shutdown(s);
        }
        ray::close_window();
        return;
    }

    if seat.is_none() && !windowed {
        println!(
            "om_wm: no session control: input grabbed, no vt switching, \
             ctrl+alt+backspace quits"
        );
    }
    // The trackpad, when it is ours to read raw rather than libinput's. libinput
    // says which node that is and when it changes, so the loop opens it on its
    // first pass and there is no second path here to keep in step.
    let mut touchpad: Option<touch::Touchpad> = None;
    // The window we are interacting with; while Some, pan/zoom is disabled.
    let mut focused: Option<WlSurface> = None;
    // Desk to start with: the canvas is the point of the thing, and arranging comes before
    // working in it.
    let mut mode = Mode::Desk;
    // A window being moved, if any.
    let mut drag: Option<Drag> = None;
    // Active Super+right-drag resize, if any, and one released and waiting to be answered.
    let mut resize: Option<Resize> = None;
    let mut resize_settle: Option<ResizeSettle> = None;
    // Windows easing back down after a drop. Each entry is the surface plus its
    // world center and z at release; we hold the visual (screen) center fixed as
    // z returns to 0 so perspective does not slide it toward the screen center.
    let mut settling: Vec<(WlSurface, f32, f32, f32)> = Vec::new();

    // Spawn a few test clients (shm terminals + dmabuf triangles) so the canvas
    // has several windows to pan and zoom around.
    let test_clients = [
        "weston-terminal",
        "weston-editor",
        "weston-clickdot",
        "weston-smoke",
    ];
    let mut children: Vec<Child> = test_clients
        .iter()
        .filter_map(|c| spawn_client(&server.socket_name, c))
        .collect();

    let start = Instant::now();
    let clear = ray::Color { r: 24, g: 24, b: 32, a: 255 };
    let mut frame: u32 = 0;
    let debug_input = std::env::var("OM_WM_DEBUG_INPUT").is_ok();
    let shot_env = std::env::var("OM_WM_SHOT");
    let screenshot = shot_env.is_ok();
    let shot_frame: u32 = shot_env
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(SHOT_FRAME);
    let max_frames: u32 = std::env::var("OM_WM_MAX_FRAMES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(u32::MAX);
    let mut last = Instant::now();
    let mut max_dt_ms: f64 = 0.0;
    // The last second of frame times, for the on-screen counter. A run-long maximum is
    // the wrong thing to watch while dragging something: one stall from startup would
    // colour it red forever.
    let mut dt_ring = [0.0f32; 60];
    let mut dt_slot = 0usize;
    let mut slow_frames: u32 = 0;
    // Last session state we acted on, to catch activation edges from libseat.
    let mut session_active = true;
    // What the cursor should be showing, as of the last frame, for the debug trace only.
    let mut last_cursor_want = "crosshair";
    // Where recent button presses landed, by serial, for anchoring client-initiated drags.
    let mut press_log: Vec<(u32, f32, f32, u32)> = Vec::new();
    // Cursor images we have seen, kept because a client may switch back to one without
    // committing it again.
    //
    // Chromium keeps a surface per cursor shape and re-uses them: it calls set_cursor on a
    // surface that committed minutes ago, so there is nothing to read at that moment, and
    // the plane went on showing the previous shape with the previous hotspot. That looks
    // like the cursor jumping while the window stays still. weston's toolkit commits every
    // time, which is why it never showed it.
    //
    // Keyed by surface, capped, and only for surfaces small enough to be a cursor.
    let mut cursor_images: Vec<(ObjectId, i32, i32, Vec<u32>)> = Vec::new();
    // What we last handed the plane: surface and hotspot, so an unchanged cursor is not
    // rebuilt every frame.
    let mut cursor_key: Option<(ObjectId, i32, i32)> = None;
    // The dragged window's geometry as last seen, to notice the client changing it.
    let mut drag_geo: Option<(f32, f32, f32, f32)> = None;
    // When the last middle button press landed, for the double click chord.
    let mut last_middle_ms: u32 = 0;
    // Surface the pointer is currently over, so crossing into another one can
    // emit leave and enter.
    let mut hovered: Option<WlSurface> = None;
    // Surface holding the implicit grab while a button is down.
    let mut grabbed: Option<WlSurface> = None;
    // Last pointer position we told a client about, in its own coordinates.
    let mut last_motion: Option<(WlSurface, f32, f32)> = None;

    while RUNNING.load(Ordering::Relaxed)
        && !ray::window_should_close()
        && frame < max_frames
    {
        // Session state, straight from libseat's callbacks. While another VT owns
        // the display we keep the protocol alive but stay off the GPU (we hold no
        // DRM master) and out of input. Clients get no frame callbacks meanwhile,
        // so they idle too.
        if let Some(s) = seat.as_ref() {
            seat::poll(s);
            let now_active = seat::active(s);
            if now_active != session_active {
                session_active = now_active;
                if now_active {
                    // The console modeset the CRTC while we were away, which
                    // leaves the cursor plane empty.
                    if let Some(c) = cursor.as_mut() {
                        cursor::rearm(c);
                    }
                    // libinput reopens the devices; input that arrived while
                    // another VT had them was not meant for us.
                    if let Some(i) = inp.as_mut() {
                        input::resume(i);
                    }
                    if let Some(tp) = touchpad.as_mut() {
                        touch::reset(tp, &set);
                    }
                    last = Instant::now();
                    println!("om_wm: back on vt{}", seat::our_vt(s));
                } else {
                    if let Some(i) = inp.as_ref() {
                        let t = start.elapsed().as_millis() as u32;
                        release_held_keys(&mut state, i, &focused, t);
                    }
                    // Let go of the devices so the session taking over can have
                    // them.
                    if let Some(i) = inp.as_mut() {
                        input::suspend(i);
                    }
                    println!("om_wm: session inactive, display released");
                }
            }
            if !now_active {
                wl::state::accept_and_dispatch(&mut server, &mut state);
                wl::state::flush(&mut server);
                std::thread::sleep(std::time::Duration::from_millis(VT_IDLE_MS));
                continue;
            }
        }

        let now = Instant::now();
        let dt_ms = now.duration_since(last).as_secs_f64() * 1000.0;
        last = now;
        dt_ring[dt_slot] = dt_ms as f32;
        dt_slot = (dt_slot + 1) % dt_ring.len();
        if frame > 5 {
            if dt_ms > max_dt_ms {
                max_dt_ms = dt_ms;
            }
            if dt_ms > 20.0 {
                slow_frames += 1;
            }
        }

        wl::state::accept_and_dispatch(&mut server, &mut state);

        for key in state.dead_dmabufs.drain(..).collect::<Vec<_>>() {
            dmabuf_cache.evict(&egl, &key);
        }
        wl::state::prune_held(&mut state);

        let committed: Vec<_> = state.committed.drain(..).collect();
        // A cursor surface commits like any other, and the window path releases the buffer
        // of anything that is not a window so the client is not left waiting on it. That
        // includes cursors, which is why reading the pixels later found nothing: they have
        // to be taken here, while the buffer is still attached, and kept.
        // Take a copy of every cursor-sized surface that commits, before the window path
        // releases its buffer. Cheap: a cursor is a few kilobytes, and there are a handful.
        for surface in &committed {
            if wl::state::is_window_like(&state, surface) {
                continue;
            }
            let id = surface.id();
            let mut taken: Option<(i32, i32, Vec<u32>)> = None;
            wl::state::with_cursor_pixels(surface, |w, h, stride, ptr| {
                if w <= 0 || h <= 0 || w > 64 || h > 64 {
                    return;
                }
                let mut pixels = vec![0u32; (w * h) as usize];
                for y in 0..h as usize {
                    let src = unsafe { ptr.add(y * stride as usize) as *const u32 };
                    let dst = &mut pixels[y * w as usize..];
                    unsafe { std::ptr::copy_nonoverlapping(src, dst.as_mut_ptr(), w as usize) };
                }
                taken = Some((w, h, pixels));
            });
            if let Some((w, h, pixels)) = taken {
                if debug_input {
                    println!("om_wm: cursor image {w}x{h} cached");
                }
                if let Some(slot) = cursor_images.iter_mut().find(|(k, ..)| *k == id) {
                    *slot = (id, w, h, pixels);
                } else {
                    // A client has a handful of shapes; a cap keeps a misbehaving one from
                    // growing this without bound.
                    if cursor_images.len() >= 32 {
                        cursor_images.remove(0);
                    }
                    cursor_images.push((id, w, h, pixels));
                }
                cursor_key = None;
            }
        }
        for surface in &committed {
            render::upload_committed(
                &mut windows,
                &mut dmabuf_cache,
                &egl,
                &mut state,
                surface,
                set.dmabuf_mode,
            );
        }

        // New windows open where the view is.
        render::set_place_origin(&mut windows, cam.cx, cam.cy);


        // Send frame callbacks and flush BEFORE the vsync-blocking draw, so the
        // client renders its next frame concurrently with our page flip instead
        // of waiting a full refresh (which would halve its frame rate).
        let time_ms = start.elapsed().as_millis() as u32;
        // Popups get frame callbacks as well as windows. A client that waits for
        // one before submitting a surface's next frame (Chromium does) otherwise
        // stalls on its first menu frame: the menu never appears and its
        // compositor complains the frame was held too long.
        // Whole trees, not just the roots: a client that renders into a subsurface asks for
        // its frame callback on that subsurface, and answering only the root leaves it
        // waiting for something that never arrives.
        for surface in wl::state::toplevel_surfaces(&state) {
            wl::state::send_frame_callbacks_tree(&surface, time_ms);
            for (popup, _, _) in wl::state::popups_of(&surface) {
                wl::state::send_frame_callbacks_tree(&popup, time_ms);
            }
        }
        wl::state::flush(&mut server);

        if let Some(i) = inp.as_mut() {
            input::poll(i);
        }

        // Pick up an edited settings file. Every value is read where it is used, per
        // event or per frame, so a save lands on the next frame with no further work.
        // Super+R reloads on demand, for when you want to know that it took.
        let forced = inp
            .as_ref()
            .map(|i| {
                input::super_down(i)
                    && input::events(i).iter().any(|&(c, p)| p && c == input::KEY_R)
            })
            .unwrap_or(false);
        if forced {
            settings::reload(&mut conf_watch, &mut set);
        } else if frame % SETTINGS_POLL_FRAMES == 0 {
            settings::reload_if_changed(&mut conf_watch, &mut set);
        }

        // Quit chord (ctrl+alt+backspace, the traditional one). While we hold the
        // keyboard grab, ctrl+c in the shell that launched us cannot reach it, so
        // without this there is no way to stop om_wm from the keyboard at all.
        if let Some(i) = inp.as_ref() {
            let quit = input::ctrl_down(i)
                && input::alt_down(i)
                && input::events(i).iter().any(|&(c, p)| p && c == input::KEY_BACKSPACE);
            if quit {
                println!("om_wm: ctrl+alt+backspace, quitting");
                RUNNING.store(false, Ordering::Relaxed);
            }
        }

        // VT switch chord (ctrl+alt+Fn, alt+left/right). The deactivation itself
        // comes back through libseat, which is where the display is released.
        let vt_target = match (seat.as_ref(), inp.as_ref()) {
            (Some(s), Some(i)) => {
                seat::chord(s, input::events(i), input::ctrl_down(i), input::alt_down(i))
            }
            _ => None,
        };
        if let (Some(target), Some(s)) = (vt_target, seat.as_ref()) {
            if seat::switch_to(s, target) {
                println!("om_wm: switching to vt{target}");
                continue;
            }
        }

        // Follow libinput on which trackpad, if any, is ours to read raw: it tells
        // us when one appears or goes away, which covers hotplug and the
        // close/reopen a session switch does.
        if inp.as_mut().map(input::trackpad_changed).unwrap_or(false) {
            if let Some(tp) = touchpad.as_mut() {
                touch::close(tp);
            }
            touchpad = inp
                .as_ref()
                .and_then(input::trackpad_node)
                .and_then(|node| touch::open(&node, &set));
        }

        let super_down = inp.as_ref().map(input::super_down).unwrap_or(false);
        // What the fingers did, not what it did to the camera: where a scroll goes is
        // decided below, alongside the wheel's, so a trackpad can scroll a window.
        let pad = match touchpad.as_mut() {
            Some(tp) => touch::update(tp, cursor.as_mut(), super_down, &set),
            None => touch::Gesture::default(),
        };
        let taps = pad.clicks;
        let (mut pressed, mut released) = (taps.left_pressed, taps.left_released);

        // Pointers, as libinput reports them: motion moves the cursor, clicks add
        // to the click edges, the wheel zooms at the cursor when unfocused. In
        // Libinput trackpad mode the scroll and pinch fields carry the trackpad
        // too; in Custom mode they stay zero because the device is muted there.
        let mut ptr = inp.as_ref().map(input::pointer).unwrap_or_default();
        // The trackpad's buttons join the mouse's, so nothing downstream has to know
        // which device a click came from. On a clickpad the right button is a region of
        // the surface (touch.rs), and it arrives here looking like a real one.
        ptr.right_pressed |= taps.right_pressed;
        ptr.right_released |= taps.right_released;
        ptr.right |= taps.right;
        ptr.left |= taps.left;
        // And its scroll joins the mouse's, in the same fields libinput fills when it
        // drives the trackpad itself. Everything downstream then treats the two modes
        // identically, including forwarding to whatever the pointer is over.
        //
        // Except while Super is held, which is reserved for moving and resizing windows:
        // a pinch in the middle of that should not also zoom the canvas. The gesture is
        // still tracked, only discarded, so letting go does not make the next one jump.
        if !super_down {
            ptr.scroll_x += pad.scroll_x;
            ptr.scroll_y += pad.scroll_y;
            ptr.pinch *= pad.pinch;
        }
        // Desk mode never hands the pointer to a client, so the canvas keeps the wheel and
        // the scroll wherever the cursor happens to be.
        let pointer_on_client = mode == Mode::Work && !super_down && hovered.is_some();
        if let Some(cur) = cursor.as_mut() {
            cursor::move_by(cur, ptr.dx as i32, ptr.dy as i32);
        }
        // Where the pointer is, in screen pixels. On DRM it is ours to track, since
        // we move the hardware cursor ourselves. Nested, the host tracks it and
        // reports it through the window, and our deltas are only there for panning.
        let pointer_xy = if windowed {
            Some(ray::mouse_position())
        } else {
            cursor.as_ref().map(cursor::pos)
        };
        // Wheel-click drag also pans; moving the cursor by the same delta keeps
        // the grabbed canvas point exactly under the cursor.
        if mode == Mode::Desk && ptr.middle && !pointer_on_client {
            cam.cx -= ptr.dx / cam.zoom;
            cam.cy -= ptr.dy / cam.zoom;
        }
        pressed |= ptr.left_pressed;
        released |= ptr.left_released;

        // Super+Escape switches mode. It used to drop keyboard focus, which desk mode makes
        // pointless (nothing is focused there) and work mode gets from clicking empty
        // canvas, so the chord is better spent on the thing you need to reach constantly.
        if let Some(i) = inp.as_ref() {
            let toggle_mode = input::super_down(i)
                && input::events(i).iter().any(|&(c, p)| p && c == input::KEY_ESC);
            if toggle_mode {
                mode = match mode {
                    Mode::Desk => Mode::Work,
                    Mode::Work => Mode::Desk,
                };
                // Leaving work mode takes the keyboard and the pointer back off the
                // clients, so nothing is left believing it still has either.
                if mode == Mode::Desk {
                    if focused.is_some() {
                        let serial = SERIAL_COUNTER.next_serial();
                        state.keyboard.clone().set_focus(&mut state, None, serial);
                        focused = None;
                    }
                    hovered = None;
                    grabbed = None;
                    last_motion = None;
                }
                println!(
                    "om_wm: {} mode",
                    if mode == Mode::Desk { "desk" } else { "work" }
                );
            }
        }

        // Debug labels on and off, so a run that started plain can answer a question
        // about sampling without being restarted.
        if let Some(i) = inp.as_ref() {
            let toggle = input::super_down(i)
                && input::events(i).iter().any(|&(c, p)| p && c == input::KEY_I);
            if toggle {
                debug_labels = !debug_labels;
                println!("om_wm: debug labels {}", if debug_labels { "on" } else { "off" });
            }
            let toggle_fps = input::super_down(i)
                && input::events(i).iter().any(|&(c, p)| p && c == input::KEY_F);
            if toggle_fps {
                debug_fps = !debug_fps;
                println!("om_wm: fps counter {}", if debug_fps { "on" } else { "off" });
            }
            let toggle_pad = input::super_down(i)
                && input::events(i).iter().any(|&(c, p)| p && c == input::KEY_P);
            if toggle_pad {
                debug_pad = !debug_pad;
                println!("om_wm: trackpad overlay {}", if debug_pad { "on" } else { "off" });
            }
        }

        // Zoom reset: Super+0 from the keyboard, Super + double middle click from
        // the mouse. The trackpad's two-finger double tap lives in touch.rs.
        let reset_key = inp
            .as_ref()
            .map(|i| {
                input::super_down(i)
                    && input::events(i).iter().any(|&(c, p)| p && c == input::KEY_0)
            })
            .unwrap_or(false);
        let mut reset_click = false;
        if ptr.middle_pressed {
            let now_ms = start.elapsed().as_millis() as u32;
            if super_down && now_ms.saturating_sub(last_middle_ms) <= set.double_click_ms {
                reset_click = true;
                // Consumed, so a third click does not reset again.
                last_middle_ms = 0;
            } else {
                last_middle_ms = now_ms;
            }
        }
        // A two-finger double tap on the trackpad asks for the same reset as Super+0.
        if mode == Mode::Desk && pad.reset_zoom && !super_down {
            camera::reset_zoom(&mut cam, &set);
        }

        // Super+0 scales around the screen center; the middle click has a cursor
        // to anchor on, so it keeps the canvas under the pointer fixed.
        if reset_click && mode == Mode::Desk {
            let (cxp, cyp) = pointer_xy.unwrap_or((0, 0));
            camera::reset_zoom_at(
                &mut cam,
                cxp as f32,
                cyp as f32,
                ray::screen_width() as f32,
                ray::screen_height() as f32,
                &set,
            );
        } else if reset_key && mode == Mode::Desk {
            camera::reset_zoom(&mut cam, &set);
        }

        // Wheel and middle-drag belong either to a client or to the canvas, never
        // to both. Super forces the canvas. This reads the hover from the frame
        // before on purpose: route_input updates it later in the frame, and using
        // one value for both branches is what stops them both firing on the frame
        // the pointer crosses a window edge.
        if mode == Mode::Desk && !pointer_on_client {
            let (cxp, cyp) = pointer_xy.unwrap_or((0, 0));
            if ptr.wheel != 0.0 {
                let wheel = if set.invert_scroll { -ptr.wheel } else { ptr.wheel };
                camera::zoom_at(
                    &mut cam,
                    1.15_f32.powf(wheel),
                    cxp as f32,
                    cyp as f32,
                    ray::screen_width() as f32,
                    ray::screen_height() as f32,
                    &set,
                );
            }
            if ptr.hwheel != 0.0 {
                let hwheel = if set.invert_scroll { -ptr.hwheel } else { ptr.hwheel };
                cam.cx += hwheel * set.hwheel_pan / cam.zoom;
            }
            // Finger scroll pans, pinch zooms at the cursor. The camera moves
            // against the fingers so the canvas travels with them, matching
            // touch.rs.
            if ptr.scroll_x != 0.0 || ptr.scroll_y != 0.0 {
                cam.cx -= ptr.scroll_x / cam.zoom;
                cam.cy += ptr.scroll_y / cam.zoom;
            }
            if ptr.pinch != 1.0 {
                camera::zoom_at(
                    &mut cam,
                    ptr.pinch,
                    cxp as f32,
                    cyp as f32,
                    ray::screen_width() as f32,
                    ray::screen_height() as f32,
                    &set,
                );
            }
        }

        // WASD and Super +/- drive the camera only in desk mode, and only when no window
        // has the keyboard, since otherwise they are someone's typing.
        if mode == Mode::Desk && focused.is_none() {
            camera::camera_update(&mut cam, inp.as_ref(), &set);
        }

        // The view lands on whole pixels every frame, whatever moved it: keys, wheel,
        // middle drag, trackpad, or a zoom reset that changed the scale without touching
        // the pan. Nothing has to remember to do it, so nothing can forget.
        camera::snap_pan(
            &mut cam,
            ray::screen_width() as f32,
            ray::screen_height() as f32,
        );

        render::prune_dead(&mut windows);
        render::animate(&mut windows, ray::frame_time());
        let cam3d = camera::camera_3d(&cam, ray::screen_height(), &set);
        let cursor_pos = pointer_xy;

        // Super+drag: grab the window under the cursor and lift it toward the
        // camera. Consumes the click so it is not also focused/forwarded. The
        // window is positioned by projecting the cursor onto the plane at its
        // current lifted z, so the grabbed point stays under the cursor at any
        // zoom (projecting onto z=0 while drawing lifted makes it out-run the
        // cursor via perspective parallax).
        let (sxp, syp) = cursor_pos
            .map(|(x, y)| (x as f32, y as f32))
            .unwrap_or((0.0, 0.0));
        // Read the socket once more before asking what clients want. The dispatch at the top
        // of the loop happens before we have even polled input; a client answering this
        // frame's press answers while we are blocked in the flip, which lands its request
        // just after that dispatch and leaves it sitting there for a whole frame. That is
        // most of the delay on a client that renders through EGL, because such a client is
        // blocked in its own swap until our flip and cannot read the press before then.
        //
        // A commit that arrives here is uploaded on the next frame rather than this one,
        // which is what already happens to anything that lands late.
        wl::state::accept_and_dispatch(&mut server, &mut state);

        // What clients asked for themselves: a titlebar drag is a move request, an edge
        // drag is a resize request. Only in work mode, where the client owns the pointer;
        // in desk mode it never saw the press that would have started one.
        //
        // Drained here, ahead of the two blocks that act on it, because a request that
        // arrives at the top of this frame and is only picked up below them does not reach
        // the quad until the next frame. That was a whole frame of the gap between grabbing
        // a titlebar and the window starting to follow, on top of the round trip we cannot
        // avoid: the press has to reach the client and its request has to come back.
        let asked_move: Vec<(WlSurface, u32)> = state.move_requests.drain(..).collect();
        let asked_resize: Vec<(WlSurface, i32, i32, u32)> = state.resize_requests.drain(..).collect();
        if mode == Mode::Work {
            if let Some((surf, serial)) = asked_move.into_iter().last() {
                // Anchor to the press the client is quoting. Between that press and this
                // request sits the client's own latency, and a pointer that kept moving
                // through it: anchoring to where the pointer is now would drag the window
                // from a point you never grabbed. Falls back to now if the serial is stale.
                let (ax, ay, af) = press_at(&press_log, serial, sxp, syp, frame);
                if debug_input {
                    let (w, h) = render::geo_size(&windows, &surf).unwrap_or((0.0, 0.0));
                    println!(
                        "om_wm: move request {}x{} {} took {} frames (press frame {af}, now {frame}), pointer travelled {:.0},{:.0} since",
                        w as i32,
                        h as i32,
                        render::source(&windows, &surf),
                        frame.saturating_sub(af),
                        sxp - ax,
                        syp - ay
                    );
                }
                if let Some((gx, gy)) = camera::screen_to_plane(cam3d, ax, ay, 0.0) {
                    // The surface position, not the visible top-left: the drag applies its
                    // offset with set_window_pos, which is in surface coordinates, and a
                    // client's geometry offset sits between the two. Mixing them moved the
                    // window by the client's shadow padding on the first frame.
                    if let Some((ox, oy)) = render::surface_origin(&windows, &surf) {
                        settling.retain(|(s, ..)| s != &surf);
                        if resize_settle.as_ref().map(|st| &st.surface) == Some(&surf) {
                            resize_settle = None;
                        }
                        render::clear_scale(&mut windows, &surf);
                        // A client-initiated move is always work mode, so no lift: just
                        // bring it to the front the way any compositor would.
                        render::front(&mut windows, &surf);
                        drag = Some(Drag {
                            surface: surf,
                            off_x: ox - gx,
                            off_y: oy - gy,
                            from_client: true,
                            start_cursor: (sxp, syp),
                            start_pos: (ox, oy),
                        });
                    }
                }
            }
            if let Some((surf, ex, ey, serial)) = asked_resize.into_iter().last() {
                let (ax, ay, af) = press_at(&press_log, serial, sxp, syp, frame);
                if debug_input {
                    let (w, h) = render::geo_size(&windows, &surf).unwrap_or((0.0, 0.0));
                    println!(
                        "om_wm: resize request {}x{} {} took {} frames (press frame {af}, now {frame}), pointer travelled {:.0},{:.0} since",
                        w as i32,
                        h as i32,
                        render::source(&windows, &surf),
                        frame.saturating_sub(af),
                        sxp - ax,
                        syp - ay
                    );
                }
                if let (Some((gx, gy)), Some((w, h)), Some((ox, oy))) = (
                    camera::screen_to_plane(cam3d, ax, ay, 0.0),
                    render::geo_size(&windows, &surf),
                    render::window_origin(&windows, &surf),
                ) {
                    // Same housekeeping the Super path does: a window still settling from
                    // the last resize is carrying a stretch, and measuring a new drag
                    // against a stretched window makes it jump.
                    if resize_settle.as_ref().map(|st| &st.surface) == Some(&surf) {
                        resize_settle = None;
                    }
                    render::clear_scale(&mut windows, &surf);
                    resize = Some(Resize {
                        surface: surf,
                        grab_x: gx,
                        grab_y: gy,
                        from_x: ox,
                        from_y: oy,
                        from_w: w,
                        from_h: h,
                        edge_x: ex as f32,
                        edge_y: ey as f32,
                        right_button: false,
                        asked: (w, h),
                        seen: (w, h),
                        waited: 0,
                        least_w: w,
                        least_h: h,
                        most_w: w,
                        most_h: h,
                        stuck_small: 0,
                        stuck_large: 0,
                    });
                }
            }
        }

        // Super+right-drag resizes. Wayland has nothing to stretch: we send the client
        // a configure with the size we want and it renders at that size in its own time,
        // clamping to whatever it declared as its minimum and maximum. So this drag
        // asks, it does not impose, and a window that stops growing has said no.
        //
        // The window's top-left stays put and the far corner follows the cursor, which
        // is the one mapping that needs no handle to grab.
        //
        // Desk mode needs no modifier: a window is an object, so dragging it moves it and
        // right dragging it resizes it. Work mode keeps both behind Super, where they are
        // the compositor's way rather than the client's.
        let grab_windows = mode == Mode::Desk || super_down;
        if ptr.right_pressed && grab_windows && resize.is_none() {
            if let Some((surf, ..)) = render::window_at(&windows, cam3d, sxp, syp) {
                if let Some((gx, gy)) = camera::screen_to_plane(cam3d, sxp, syp, 0.0) {
                    let root = wl::state::window_root(&mut state, &surf);
                    if let Some((w, h)) = render::geo_size(&windows, &root) {
                        // A window still settling from a previous resize starts fresh.
                        if resize_settle.as_ref().map(|s| &s.surface) == Some(&root) {
                            resize_settle = None;
                        }
                        render::clear_scale(&mut windows, &root);
                        let (ox, oy) = render::window_origin(&windows, &root).unwrap_or((0.0, 0.0));
                        resize = Some(Resize {
                            surface: root,
                            grab_x: gx,
                            grab_y: gy,
                            from_x: ox,
                            from_y: oy,
                            from_w: w,
                            from_h: h,
                            edge_x: 1.0,
                            edge_y: 1.0,
                            right_button: true,
                            asked: (w, h),
                            seen: (w, h),
                            waited: 0,
                            least_w: w,
                            least_h: h,
                            most_w: w,
                            most_h: h,
                            stuck_small: 0,
                            stuck_large: 0,
                        });
                    }
                }
            }
        }
        if let Some(r) = resize.as_mut() {
            let alive = r.surface.is_alive();
            // Canvas units are surface pixels at zoom 1, and projecting the cursor
            // already divides by the zoom, so the corner tracks the cursor at any zoom.
            // Where the cursor says the corner is, held back at whatever the client has
            // shown it cannot pass. Written as min-then-max rather than clamp: bounds
            // learned at runtime can cross, and clamp panics when they do.
            let floor_w = if r.stuck_small >= RESIZE_STUCK_COMMITS { r.least_w } else { set.resize_min_px };
            let floor_h = if r.stuck_small >= RESIZE_STUCK_COMMITS { r.least_h } else { set.resize_min_px };
            let ceil_w = if r.stuck_large >= RESIZE_STUCK_COMMITS { r.most_w } else { f32::MAX };
            let ceil_h = if r.stuck_large >= RESIZE_STUCK_COMMITS { r.most_h } else { f32::MAX };
            let want = camera::screen_to_plane(cam3d, sxp, syp, 0.0).map(|(px, py)| {
                // An edge direction of zero pins that axis; -1 means the near edge is the
                // one under the cursor, so moving it left or up makes the window bigger.
                let dw = r.edge_x * (px - r.grab_x);
                let dh = r.edge_y * (py - r.grab_y);
                (
                    (r.from_w + dw).min(ceil_w).max(floor_w.min(ceil_w)),
                    (r.from_h + dh).min(ceil_h).max(floor_h.min(ceil_h)),
                )
            });
            let held = if r.right_button { ptr.right } else { ptr.left };
            match (held && alive, want) {
                (true, Some((w, h))) => {
                    // The quad shows where the cursor is, scaled against whatever the
                    // client has committed so far, so its commits change the pixels
                    // without moving the corner.
                    // With the stretch off, nothing moves until the client answers, which
                    // is the conventional behaviour and what trails your hand on a slow
                    // client. Both are worth being able to feel, so it is a setting.
                    let (gw, gh) = render::geo_size(&windows, &r.surface).unwrap_or((w, h));
                    if set.resize_stretch {
                        render::set_scale(&mut windows, &r.surface, w / gw, h / gh);
                    }
                    // Dragging a near edge keeps the far one still, which means the window
                    // has to travel as it grows. Derived from the size actually in force
                    // rather than accumulated, so a client that clamps cannot make the
                    // still edge drift.
                    if r.edge_x < 0.0 || r.edge_y < 0.0 {
                        let x = if r.edge_x < 0.0 { r.from_x + r.from_w - w } else { r.from_x };
                        let y = if r.edge_y < 0.0 { r.from_y + r.from_h - h } else { r.from_y };
                        render::set_window_origin(&mut windows, &r.surface, x, y);
                    }

                    // And keep it rendering. Asking again only when it has answered means
                    // we never queue a size it will not get to, and the timeout keeps a
                    // client that ignores configures (weston-editor does) from stalling
                    // the stream for the rest of the drag.
                    let answered = (gw, gh) != r.seen;
                    // Learn from what the client manages. Every commit either makes
                    // progress in the direction being dragged, which clears the counter,
                    // or does not, and enough of those in a row mean it has stopped.
                    if answered {
                        let smaller = gw + RESIZE_PROGRESS_PX < r.least_w
                            || gh + RESIZE_PROGRESS_PX < r.least_h;
                        let larger = gw > r.most_w + RESIZE_PROGRESS_PX
                            || gh > r.most_h + RESIZE_PROGRESS_PX;
                        if smaller {
                            r.stuck_small = 0;
                        } else if w < r.least_w - RESIZE_PROGRESS_PX {
                            // We are asking for smaller and it did not get smaller.
                            r.stuck_small += 1;
                        }
                        if larger {
                            r.stuck_large = 0;
                        } else if w > r.most_w + RESIZE_PROGRESS_PX {
                            r.stuck_large += 1;
                        }
                        r.least_w = r.least_w.min(gw);
                        r.least_h = r.least_h.min(gh);
                        r.most_w = r.most_w.max(gw);
                        r.most_h = r.most_h.max(gh);
                    }
                    let moved = (w - r.asked.0).abs() >= 1.0 || (h - r.asked.1).abs() >= 1.0;
                    r.waited += 1;
                    if moved && (answered || r.waited >= set.resize_wait_frames) {
                        wl::state::resize_toplevel(&state, &r.surface, w as i32, h as i32);
                        r.asked = (w, h);
                        r.seen = (gw, gh);
                        r.waited = 0;
                    }
                }
                _ => {
                    // Released: ask once, for the size the stretch is showing, and keep
                    // the stretch until the client answers so the window does not snap
                    // back to its old size for a frame.
                    if alive {
                        if let Some((w, h)) = want {
                            let was = render::geo_size(&windows, &r.surface)
                                .unwrap_or((r.from_w, r.from_h));
                            wl::state::resize_toplevel(&state, &r.surface, w as i32, h as i32);
                            resize_settle = Some(ResizeSettle {
                                surface: r.surface.clone(),
                                was,
                                frames: 0,
                            });
                        }
                    }
                    resize = None;
                }
            }
        }
        // Take the stretch off once the client has committed something new, or give up
        // waiting. A client that refuses the size (weston-editor does) leaves us
        // stretching a window that is never going to grow, and snapping back to the
        // truth is better than lying about it indefinitely.
        if let Some(st) = resize_settle.as_mut() {
            let now = render::geo_size(&windows, &st.surface);
            st.frames += 1;
            let answered = now.map(|n| n != st.was).unwrap_or(true);
            if answered || st.frames >= RESIZE_SETTLE_FRAMES {
                render::clear_scale(&mut windows, &st.surface);
                resize_settle = None;
            }
        }

        if pressed && grab_windows && drag.is_none() {
            if let Some((surf, ox, oy)) =
                render::window_at(&windows, cam3d, sxp, syp)
            {
                // Re-grabbing a still-settling window: cancel its settle.
                settling.retain(|(s, ..)| s != &surf);
                // Offset from the window origin to the grabbed point, captured
                // on the z=0 plane (constant in world units, independent of z).
                let (gx, gy) = camera::screen_to_plane(cam3d, sxp, syp, 0.0)
                    .unwrap_or((ox, oy));
                // Desk mode lifts a window toward the camera, which is the canvas idiom
                // for picking something up. Work mode is a normal compositor: raise it in
                // the stack and leave it on the plane, so nothing about the geometry
                // changes while you drag it.
                if mode == Mode::Desk {
                    render::raise(&mut windows, &surf);
                } else {
                    render::front(&mut windows, &surf);
                }
                drag = Some(Drag {
                    surface: surf,
                    off_x: ox - gx,
                    off_y: oy - gy,
                    from_client: false,
                    start_cursor: (sxp, syp),
                    start_pos: (ox, oy),
                });
                pressed = false;
            }
        }
        if let Some(d) = drag.as_ref() {
            let surf = d.surface.clone();
            let (offx, offy, from_client) = (d.off_x, d.off_y, d.from_client);
            let (d_start_cursor, d_start_pos) = (d.start_cursor, d.start_pos);
            if debug_input {
                // The client changing its own geometry mid-drag would move the window
                // without anything here moving it, since our position is the surface's and
                // the visible top-left is that plus the geometry offset.
                let geo = wl::state::geometry_of(&surf);
                if geo != drag_geo {
                    println!("om_wm: drag geometry now {geo:?} (was {drag_geo:?})");
                    drag_geo = geo;
                }
            }
            let z = render::window_z(&windows, &surf).unwrap_or(0.0);
            if let Some((px, py)) = camera::screen_to_plane(cam3d, sxp, syp, z) {
                render::set_window_pos(&mut windows, &surf, px + offx, py + offy);
            }
            if released || !surf.is_alive() {
                // Snapshot the visual center so it stays put while lowering.
                if surf.is_alive() {
                    if let (Some((cx, cy)), Some(z)) = (
                        render::window_center(&windows, &surf),
                        render::window_z(&windows, &surf),
                    ) {
                        // Only a lifted window has anything to come down from.
                        if z.abs() > 0.5 {
                            settling.push((surf.clone(), cx, cy, z));
                        }
                    }
                }
                render::settle(&mut windows, &surf);
                if debug_input {
                    // Did the window travel as far as the cursor? On screen it should, at
                    // any zoom: the cursor delta in canvas units is the screen delta over
                    // the zoom, and the window moves in canvas units.
                    let (cx0, cy0) = d_start_cursor;
                    let (px0, py0) = d_start_pos;
                    let now = render::surface_origin(&windows, &surf).unwrap_or((0.0, 0.0));
                    let cursor_d = (sxp - cx0, syp - cy0);
                    let win_d = ((now.0 - px0) * cam.zoom, (now.1 - py0) * cam.zoom);
                    println!(
                        "om_wm: drag end from_client={from_client} zoom {:.3} cursor moved {:.1},{:.1} window moved {:.1},{:.1} (screen px) drift {:.1},{:.1}",
                        cam.zoom,
                        cursor_d.0, cursor_d.1,
                        win_d.0, win_d.1,
                        win_d.0 - cursor_d.0, win_d.1 - cursor_d.1
                    );
                    drag_geo = None;
                }
                drag = None;
                // The client's own drag needs its release; ours must not become a click.
                if !from_client {
                    released = false;
                }
            }
        }

        // Ease dropped windows straight down: reproject the release-time screen
        // center onto the plane at each window's current z, so it lowers in
        // place instead of sliding toward the screen center. dist and the camera
        // center come straight from the perspective camera.
        {
            let px = cam3d.position.x;
            let py = cam3d.position.y;
            let dist = -cam3d.position.z;
            settling.retain(|(surf, cx0, cy0, z0)| {
                if !surf.is_alive() {
                    return false;
                }
                let Some(z) = render::window_z(&windows, surf) else {
                    return false;
                };
                let keep = z.abs() > 0.5;
                let zz = if keep { z } else { 0.0 };
                let k = (zz + dist) / (*z0 + dist);
                let cx = px + (*cx0 - px) * k;
                let cy = py + (*cy0 - py) * k;
                render::set_window_center(&mut windows, surf, cx, cy);
                keep
            });
        }

        // The cursor image. Desk mode is ours: the crosshair says the canvas has the
        // pointer, which is exactly what is true there. Work mode is the client's, because
        // a text field wants an I-beam and a resize edge wants an arrow, and the client is
        // the only one that knows which. A client asking for no cursor gets none.
        //
        // The hotspot comes with it, which is the part that was wrong before: a cursor
        // image says where in itself the pointer actually is, and without honouring that
        // the click lands somewhere other than the tip.
        // Hand the plane whatever the client is currently asking for, from the cache. The
        // hotspot is read live, because set_cursor can change it without the surface
        // committing anything.
        if let (Some(cur), CursorImageStatus::Surface(surf)) = (cursor.as_mut(), &state.cursor_image)
        {
            let id = surf.id();
            let (hx, hy) = wl::state::cursor_hotspot(surf);
            if cursor_key.as_ref() != Some(&(id.clone(), hx, hy)) {
                if let Some((_, w, h, pixels)) = cursor_images.iter().find(|(k, ..)| *k == id) {
                    if debug_input {
                        println!("om_wm: cursor now {w}x{h} hotspot {hx},{hy}");
                    }
                    cursor::store_client_image(
                        cur,
                        *w,
                        *h,
                        w * 4,
                        pixels.as_ptr() as *const u8,
                        hx,
                        hy,
                    );
                    cursor_key = Some((id, hx, hy));
                }
            }
        }

        if let Some(cur) = cursor.as_mut() {
            let client_cursor = mode == Mode::Work && hovered.is_some();
            if debug_input {
                let want = match (client_cursor, &state.cursor_image) {
                    (true, CursorImageStatus::Surface(_)) => "client",
                    (true, CursorImageStatus::Hidden) => "hidden",
                    (true, CursorImageStatus::Named(_)) => "named",
                    _ => "crosshair",
                };
                if want != last_cursor_want {
                    println!("om_wm: cursor want {want} (was {last_cursor_want})");
                    last_cursor_want = want;
                }
            }
            match (client_cursor, &state.cursor_image) {
                (true, CursorImageStatus::Surface(_)) if cursor::has_client_image(cur) => {
                    cursor::apply_client(cur);
                }
                (true, CursorImageStatus::Hidden) => cursor::set_hidden(cur),
                // A named shape, which needs a cursor theme we do not load yet; a surface
                // whose pixels we have not seen yet; and anything at all in desk mode.
                _ => cursor::set_crosshair(cur),
            }
        }

        // Menus and subsurfaces are anchored to their root, so they are placed after
        // everything that could have moved that root: the drag, the settle, the client's own
        // move and resize requests.
        //
        // This used to run before all of that, which meant a window's children were placed
        // from where it was a frame ago. Invisible on a client without subsurfaces, and
        // visible on Chromium as part of the window trailing the rest of it while dragging.
        render::sync_children(&mut windows);

        // Everything that could move a window has run. Put them all on the pixel grid, then
        // decide how each one is sampled from where it actually ended up.
        render::align_positions(&mut windows, cam.zoom);
        render::prepare_textures(&mut windows, cam.zoom, anisotropy);

        // Dismissal is the popup grab's job now, not ours: it knows the chain and
        // tells the client in the right order. OM_WM_DEBUG_INPUT=1 still dumps the
        // click and every rect it could have hit.
        if debug_input && pressed {
            println!("om_wm: click screen {sxp:.0},{syp:.0}");
            render::log_rects(&windows);
        }

        // Clients hear from us only in work mode. In desk mode they get no pointer events
        // and no keys at all: a click there means "grab this window", and there is nothing
        // focused to type into.
        let time_ms = start.elapsed().as_millis() as u32;
        if mode == Mode::Work {
            route_input(
                &mut state,
                &mut windows,
                cam3d,
                cursor_pos,
                &mut focused,
                &mut hovered,
                &mut grabbed,
                &mut last_motion,
                ptr.left || ptr.right || ptr.middle,
                inp.as_ref(),
                debug_input,
                super_down,
                drag.as_ref().map(|d| d.from_client).unwrap_or(false)
                    || resize.as_ref().map(|r| !r.right_button).unwrap_or(false),
                &mut press_log,
                &ptr,
                pressed,
                released,
                time_ms,
                frame,
            );
            if pointer_on_client {
                forward_scroll(&mut state, &ptr, time_ms, &set);
            }
        }
        // Outside the mode gate on purpose: xkb state has to follow the keyboard even when
        // no client is listening, or the next client to get focus inherits modifiers that
        // nobody is holding.
        if let Some(i) = inp.as_ref() {
            forward_keys(&mut state, i, time_ms);
        }

        // Everything routed this frame goes out now, not on the next iteration.
        //
        // The flush that sends frame callbacks happens near the top of the loop, which is
        // right for callbacks: a client then renders while we are blocked presenting. But
        // this frame's pointer events, keys and resize configures are produced after it, so
        // they used to sit in the buffer through the blocking present and reach the client as
        // the next frame began. A frame of latency on everything we say to a client, for want
        // of a flush.
        wl::state::flush(&mut server);

        // While a window is following the pointer, the cursor plane waits for the frame that
        // window is in: the plane would otherwise be a frame ahead of it, which is the whole
        // of what "the window lags the cursor" is.
        if let Some(cur) = cursor.as_mut() {
            cursor::set_deferred(cur, drag.is_some() || resize.is_some());
        }

        ray::begin_drawing();
        ray::clear_background(clear);
        render::draw_windows(&windows, cam3d, shader, alpha_loc, swizzle_loc);
        render::draw_mode_badge(
            if mode == Mode::Desk { "desk" } else { "work" },
            mode == Mode::Desk,
            ray::screen_width(),
        );
        if debug_labels {
            render::draw_debug_labels(&windows, cam3d, cam.zoom, anisotropy);
        }
        if debug_fps {
            // Averaged for the rate, worst for the stutter. Skip empty slots so the
            // first second on screen is not distorted by zeros.
            let mut sum = 0.0f32;
            let mut worst = 0.0f32;
            let mut n = 0.0f32;
            for &v in dt_ring.iter() {
                if v > 0.0 {
                    sum += v;
                    worst = worst.max(v);
                    n += 1.0;
                }
            }
            let avg = if n > 0.0 { sum / n } else { 0.0 };
            let fps = if avg > 0.0 { 1000.0 / avg } else { 0.0 };
            render::draw_frame_stats(fps, dt_ms as f32, worst);
        }
        if debug_pad {
            if let Some(tp) = touchpad.as_ref() {
                render::draw_pad_debug(
                    &touch::view(tp, &set),
                    ray::screen_width(),
                    ray::screen_height(),
                );
            }
        }
        if screenshot && frame == shot_frame {
            ray::flush_batch();
            ray::take_screenshot("shot.png");
        }
        // Just before the present, so the plane and the frame land together.
        if let Some(cur) = cursor.as_mut() {
            cursor::present_deferred(cur);
        }
        ray::end_drawing();


        frame += 1;
    }

    println!(
        "om_wm: shutting down after {frame} frames; cached dmabufs={} max_frame={:.1}ms slow_frames(>20ms)={}",
        dmabuf_cache.key.len(),
        max_dt_ms,
        slow_frames
    );
    for mut c in children.drain(..) {
        let _ = c.kill();
        let _ = c.wait();
    }
    render::destroy_owned(&mut windows);
    dmabuf_cache.destroy_all(&egl);
    if let Some(c) = cursor.as_mut() {
        cursor::destroy(c);
    }
    ray::unload_shader(shader);
    ray::close_window();
    // Release the session last: logind restores our VT to a text console when the
    // connection goes.
    if let Some(s) = seat.as_mut() {
        seat::shutdown(s);
    }
}
