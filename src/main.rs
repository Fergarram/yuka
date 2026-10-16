//
// yuka
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
mod lua;
mod ray;
mod render;
mod settings;
mod seat;
mod touch;
mod wl;
mod xcursor;

use std::ffi::c_int;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use smithay::backend::input::{Axis, AxisSource, ButtonState, KeyState};
use smithay::input::keyboard::{FilterResult, Keycode};
use smithay::input::pointer::CursorImageStatus;
use smithay::input::pointer::{
    AxisFrame, ButtonEvent, GesturePinchBeginEvent, GesturePinchEndEvent,
    GesturePinchUpdateEvent, MotionEvent,
};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::backend::ObjectId;
use smithay::reexports::wayland_server::Resource;
use smithay::utils::{Logical, Point, SERIAL_COUNTER};

use render::{DmabufMode, Windows};
use wl::state::State;

//
// Constants
//

// Default frame for YUKA_SHOT; YUKA_SHOT=<n> picks another, which matters when
// what you want to see only exists a few seconds in.
const SHOT_FRAME: u32 = 200;
const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;
// How long to keep a released resize stretched while waiting for the client to commit
// the size we asked for. A client that ignores the ask gets snapped back after this, so
// a stretched window is never left lying about its size.
const RESIZE_SETTLE_FRAMES: u32 = 90;
// How often to look for a saved script. Once a second is instant to a human hitting
// save, and 60 directory reads a minute is nothing next to a frame.
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

// The pointer belongs to the client, and Super borrows it for the canvas.
//
// There were two modes before this, desk and work, differing in who owned the pointer, and a
// badge on screen because a mode you cannot see will surprise you. The badge was the tell:
// nearly all the time was spent in work mode, and desk mode was a place you visited to pan
// and then left. So the canvas moved behind Super instead, which is where moving and resizing
// a window already lived, and the mode went away with the thing it was for.
//
// Held, Super means the canvas: two fingers pan and pinch it, the wheel zooms it, a drag moves
// a window and a right drag resizes it. Released, everything belongs to whatever is under the
// pointer, which is what a compositor is supposed to do.

// A cursor image a client has committed, and where the pointer is inside it.
//
// The hotspot is ours rather than the client's copy, because the two are in different
// coordinate systems. set_cursor gives one in surface coordinates and wl_surface.attach moves
// the buffer around inside that surface, so what the plane needs, the hotspot in buffer pixels,
// is the client's minus everything the attaches have moved. Kept per surface: a client reuses
// one cursor surface for every shape it shows.
struct CursorImage {
    id: ObjectId,
    w: i32,
    h: i32,
    pixels: Vec<u32>,
    // In buffer pixels, which is what the plane is armed with.
    hot: (i32, i32),
    // And which set_cursor we last took a hotspot from, by the serial the protocol side counts.
    //
    // The call rather than the value. A client that states the same hotspot twice has stated it
    // twice, and each statement resets whatever the attaches since had subtracted from it; reading
    // that off the value instead meant a client repeating one hotspot never reset at all, and every
    // attach offset piled onto it until the tip was in the wrong place.
    set: u32,
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

// An in-progress resize, ours or one a client asked for.
//
// A resize on Wayland cannot be immediate: the configure goes out, the client re-renders,
// commits, we import and upload, we draw. The client is asked for a new size as fast as it
// can answer, so its content keeps reflowing at whatever rate it manages, and the window is
// whatever size it has committed. The corner reaches your cursor when the client says it
// has, which means nothing on screen is ever a size the window is not.
//
// resize_stretch trades that away for a corner that tracks your hand exactly, by scaling the
// quad to the cursor while the client catches up. Off by default: the cost is showing content
// at a size it was not drawn at, and having to take the difference back whenever the client
// declines to follow.
//
// Sizes are computed from where the drag was grabbed rather than accumulated per frame, so
// nothing drifts, and they are recomputed against whatever the client has actually committed,
// so a commit mid-drag changes the window without moving the edge being held still.
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
            println!("yuka: spawned '{cmd}' (pid {})", child.id());
            Some(child)
        }
        Err(e) => {
            eprintln!("yuka: failed to spawn '{cmd}': {e}");
            None
        }
    }
}

// Settings from a script load replace the ones in force, except trackpad_mode: reopening the
// device is not something a file save should do, so the mode stays until the next start.
fn adopt_settings(set: &mut settings::Settings, new: settings::Settings) {
    let mode = set.trackpad_mode;
    if new.trackpad_mode != mode {
        eprintln!("yuka: settings: trackpad_mode change takes effect on restart");
    }
    *set = new;
    set.trackpad_mode = mode;
}

// Whether two surfaces belong to the same client, which is what separates a dialog opening
// in front of you from another application taking your keystrokes.
fn same_client(a: &WlSurface, b: &WlSurface) -> bool {
    match (a.client(), b.client()) {
        (Some(x), Some(y)) => x.id() == y.id(),
        _ => false,
    }
}

// Move the keyboard, and tell both windows involved.
//
// Focus is two facts, not one: Smithay routes key events to whoever holds it, and the client
// draws itself differently depending on whether it has it. Only the first was being done, so
// every window drew its chrome greyed out with no focus ring, forever, however focused it
// actually was. Activated is the only way a client can know, and nothing was setting it.
//
// One place for all of it, because the focus changes in four (a press on a window, a press on
// empty canvas, and Super+Escape) and two of them would have forgotten.
fn focus_window(state: &mut State, focused: &mut Option<WlSurface>, next: Option<WlSurface>) {
    // Against what the seat actually holds, not only against what we last asked for. A grab
    // is allowed to refuse a focus change and a popup grab does exactly that, so our idea of
    // who is focused can be a window the keyboard never went to. Comparing the two means a
    // refused change is noticed and asked for again once the grab lets go, instead of being
    // skipped forever by a guard that thinks the job is already done.
    if *focused == next && state.keyboard.current_focus() == next {
        return;
    }
    if let Some(old) = focused.as_ref() {
        wl::state::set_activated(state, old, false);
    }
    if let Some(new) = next.as_ref() {
        wl::state::set_activated(state, new, true);
    }
    let serial = SERIAL_COUNTER.next_serial();
    state.keyboard.clone().set_focus(state, next.clone(), serial);
    if next.is_none() {
        // Setting focus to nothing does not reach the seat's focus hook, so the clipboard has
        // to be told separately or it stays pointed at the window that just lost it.
        wl::state::clear_clipboard_focus(state);
    }
    *focused = next;
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
    debug: bool,
    // Whether clients get the pointer this frame. When not (Super held, by default) the canvas
    // has it.
    to_clients: bool,
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
    // True on the frame the scripts asked to let go of everything, or three fingers tapped.
    let_go: bool,
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

    // Letting go of everything, decided by the caller and applied here to the input half.
    //
    // The keyboard, which also takes the clipboard offer and the window's Activated state with
    // it. The pointer, so nothing is left believing the cursor is inside it. And the implicit
    // grab, which is the one that used to survive: a button held when the chord was pressed
    // kept every later event pointed at whatever it started on, so releasing Super handed the
    // client back events it should never have seen.
    //
    // Nothing else this frame: everything was asked to be let go of.
    if let_go {
        focus_window(state, focused, None);
        *grabbed = None;
        if hovered.is_some() {
            let serial = SERIAL_COUNTER.next_serial();
            pointer.motion(state, None, &MotionEvent { location: loc, serial, time: time_ms });
            pointer.frame(state);
            *hovered = None;
            *last_motion = None;
        }
        return;
    }

    // The canvas claims the pointer: no hover, no buttons, no scroll for clients, so zooming
    // and dragging never make the thing under the cursor react. Pointer focus is withdrawn on
    // the way in, so nothing is left believing the cursor is still inside it.
    if !to_clients {
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
                "yuka: press -> surface local {:.0},{:.0} (origin {ox:.0},{oy:.0})",
                ccx - ox,
                ccy - oy
            ),
            None => println!("yuka: press -> nothing under the pointer"),
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

    // Keyboard focus is not decided here. Clicking to focus is the scripts' (yk.focus), and it
    // has already happened by the time the press is sent below.

    let mut sent = false;
    for (button, down, up) in edges {
        if !down && !up {
            continue;
        }
        if under.is_none() {
            if debug {
                println!("yuka: button {button:#x} had nothing under the pointer");
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
// Hand a pinch to the window under the pointer, as the sequence the protocol describes
// rather than as scroll it would have to guess at.
//
// A pinch the canvas is not using is currently thrown away: the pad recognises it, the camera
// ignores it unless Super is held or we are out in the overview, and the client never hears
// about it. That is a browser unable to zoom a page with two fingers while every other
// gesture works.
//
// A sequence has a beginning, a middle and an end, and the middle carries the scale relative
// to the beginning rather than to the frame before, so the total is accumulated here. The pad
// reports a per-frame ratio because that is what the camera wants.
//
// active and scale belong to the caller because the sequence outlives a frame.
#[allow(clippy::too_many_arguments)]
fn forward_pinch(
    state: &mut State,
    started: bool,
    ended: bool,
    factor: f32,
    to_client: bool,
    active: &mut bool,
    scale: &mut f64,
    time_ms: u32,
) {
    let pointer = state.pointer.clone();
    if started && to_client && !*active {
        *active = true;
        *scale = 1.0;
        let serial = SERIAL_COUNTER.next_serial();
        pointer.gesture_pinch_begin(
            state,
            &GesturePinchBeginEvent { serial, time: time_ms, fingers: 2 },
        );
    }
    if !*active {
        return;
    }
    // The pointer leaving the window, or the canvas taking the gesture over, ends the
    // sequence as cancelled: the client is told it did not finish rather than left waiting.
    if !to_client {
        *active = false;
        let serial = SERIAL_COUNTER.next_serial();
        pointer.gesture_pinch_end(
            state,
            &GesturePinchEndEvent { serial, time: time_ms, cancelled: true },
        );
        return;
    }
    if factor != 1.0 {
        *scale *= factor as f64;
        pointer.gesture_pinch_update(
            state,
            &GesturePinchUpdateEvent {
                time: time_ms,
                // The centre of the gesture does not move while the pad calls it a pinch:
                // travel is what makes it a pan instead, and the two are decided apart.
                delta: (0.0, 0.0).into(),
                scale: *scale,
                // Not measured. Two fingers on this pad give a distance and a centre, and
                // nothing has asked for the angle between them yet.
                rotation: 0.0,
            },
        );
    }
    if ended {
        *active = false;
        let serial = SERIAL_COUNTER.next_serial();
        pointer.gesture_pinch_end(
            state,
            &GesturePinchEndEvent { serial, time: time_ms, cancelled: false },
        );
    }
}

// Which axes of a finger scroll a client is currently being told about.
//
// Two fingers travelling up a pad also wander sideways, by a pixel here and a fraction there,
// and the canvas is happy to take both: panning diagonally is a thing you do. A client is not
// the canvas. Libinput locks a touchpad scroll to one axis, and a terminal that scrolls under
// libinput and sits dead under our own gestures is a terminal being handed a wandering second
// axis it has to resolve against the first.
//
// But a browser and a map do want both, so the lock is a starting position rather than a
// verdict: whichever axis leads at the start holds the gesture alone, and the moment the other
// one is doing real work rather than wandering, both are sent for the rest of the gesture. The
// camera's copy is never locked at all.
#[derive(Clone, Copy, PartialEq)]
enum ScrollLock {
    None,
    Vertical,
    Horizontal,
    Both,
}

// How much of the recent gesture the axis test remembers. Multiplying the running travel by
// this once a frame makes it a window of roughly ten frames rather than the whole gesture, so
// a scroll that turns a corner without lifting is judged on what the fingers are doing now.
const SCROLL_TRAVEL_DECAY: f32 = 0.9;

// A finger scroll in progress toward a client: which axes it is sending, and how far each has
// travelled lately. It outlives a frame, so the main loop owns it.
struct ScrollGesture {
    lock: ScrollLock,
    travel_x: f32,
    travel_y: f32,
    // Where the gesture goes is not in here: the scripts decide it, and hold it for the whole
    // gesture (see lua/init.lua). This is only what sending it to a client involves.
    // Whether we have actually sent this client axis values during this gesture, so an axis
    // sequence is open toward it.
    //
    // The stop is what closes one. A client that received no values has nothing open, and telling
    // it a sequence ended anyway is not harmless: GTK takes the end of a scroll as the moment to
    // release its kinetic scrolling, so an unfocused window that heard nothing all gesture still
    // runs the inertia it was holding the instant we say stop. Which is a canvas pan leaking into
    // a window that was never scrolled.
    opened: bool,
}

fn forward_scroll(
    state: &mut State,
    ptr: &input::Pointer,
    // Whether the wheel belongs to the client. Asked per event, because a notch is whole.
    wheel_to_client: bool,
    // And whether the finger scroll does, which is the latched answer for the whole gesture.
    fingers_to_client: bool,
    // True on the frame a finger scroll finished, which has to be said out loud.
    scroll_ended: bool,
    // The gesture in progress: which axes it is sending and how far each has travelled.
    gesture: &mut ScrollGesture,
    time_ms: u32,
    set: &settings::Settings,
) {
    let pointer = state.pointer.clone();
    let scroll_sign = settings::client_scroll_sign(set);
    let hscroll_sign = settings::client_hscroll_sign(set);
    if wheel_to_client && (ptr.wheel != 0.0 || ptr.hwheel != 0.0) {
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
    // Values only for the side that owns the gesture. The stop below is sent either way, and
    // this frame can be both the last of a canvas pan and the frame the stop goes out on: without
    // this test the tail of a pan the canvas owned lands in the window under the cursor, which is
    // the whole bug this latch exists for, arriving one frame later than before.
    if fingers_to_client && (ptr.scroll_x != 0.0 || ptr.scroll_y != 0.0) {
        // Finger scroll into a window gets its own sensitivity: the same gesture serves
        // the canvas, where one to one is right, and a page, which usually wants more.
        let sens = set.window_scroll_sens;
        // What the fingers have been doing lately, on each axis, as a decaying sum.
        gesture.travel_x = gesture.travel_x * SCROLL_TRAVEL_DECAY + ptr.scroll_x.abs();
        gesture.travel_y = gesture.travel_y * SCROLL_TRAVEL_DECAY + ptr.scroll_y.abs();
        // The first frame of a gesture decides which axis holds it. Whichever moved further
        // wins, and a tie goes to the vertical, which is the one nearly every scroll is.
        if gesture.lock == ScrollLock::None {
            gesture.lock = if ptr.scroll_y.abs() >= ptr.scroll_x.abs() {
                ScrollLock::Vertical
            } else {
                ScrollLock::Horizontal
            };
        }
        // And the minor axis takes it back as soon as it is doing real work rather than
        // wandering. Once both are going they stay going: a gesture that has been diagonal is
        // a two-dimensional one, and dropping an axis under a hand still moving is worse than
        // carrying one it has stopped using.
        let frac = set.scroll_axis_lock_frac;
        match gesture.lock {
            ScrollLock::Vertical if gesture.travel_x >= gesture.travel_y * frac => {
                gesture.lock = ScrollLock::Both;
            }
            ScrollLock::Horizontal if gesture.travel_y >= gesture.travel_x * frac => {
                gesture.lock = ScrollLock::Both;
            }
            _ => {}
        }
        let send_v = gesture.lock == ScrollLock::Vertical || gesture.lock == ScrollLock::Both;
        let send_h = gesture.lock == ScrollLock::Horizontal || gesture.lock == ScrollLock::Both;
        let v = if send_v { ptr.scroll_y * scroll_sign * sens } else { 0.0 };
        let h = if send_h { ptr.scroll_x * hscroll_sign * sens } else { 0.0 };
        if v != 0.0 || h != 0.0 {
            let mut frame = AxisFrame::new(time_ms).source(AxisSource::Finger);
            if v != 0.0 {
                frame = frame.value(Axis::Vertical, v as f64);
            }
            if h != 0.0 {
                frame = frame.value(Axis::Horizontal, h as f64);
            }
            pointer.axis(state, frame);
            pointer.frame(state);
            gesture.opened = true;
        }
    }
    // The end of a finger scroll is an event of its own, and the protocol requires it for
    // this source: a client cannot see the pad, so a sequence never ended is one it goes on
    // believing is running. Firefox is the one that showed it, by ignoring scrolls that came
    // after a sequence it thought was still open.
    //
    // Only for a sequence we opened, though. See ScrollGesture::opened.
    //
    // Both axes, whichever was moving. Stopping one that was not is nothing to a client, and
    // working out which were live is bookkeeping for no gain.
    if scroll_ended {
        gesture.lock = ScrollLock::None;
        gesture.travel_x = 0.0;
        gesture.travel_y = 0.0;
        if gesture.opened {
            gesture.opened = false;
            let frame = AxisFrame::new(time_ms)
                .source(AxisSource::Finger)
                .stop(Axis::Vertical)
                .stop(Axis::Horizontal);
            pointer.axis(state, frame);
            pointer.frame(state);
        }
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
// sends nothing, which is exactly what an empty canvas wants.
// Forward keys to the focused client, unless the scripts are keeping this frame's presses
// (Route::keys; while Super is held, by default).
//
// Modifiers themselves still go. They are not typing, and xkb's idea of what is held has to
// follow the keyboard or the next client to take focus inherits modifiers nobody is pressing.
//
// sent is what we have told the client is down, so a release only goes where its press went.
// Without it, holding a letter, then pressing Super, then letting the letter go leaves the
// client believing that key is still held: it never hears the release, because by then the
// presses were being kept. That is a key repeating forever in someone's editor.
fn forward_keys(
    state: &mut State,
    kb: &input::Input,
    // Whether presses reach the client this frame. Not while Super is held, by default.
    typing: bool,
    sent: &mut [bool; input::KEY_CODES],
    time_ms: u32,
) {
    let keyboard = state.keyboard.clone();
    for &(code, pressed) in input::events(kb) {
        let i = code as usize;
        if !input::is_modifier(code) {
            if pressed {
                if !typing {
                    continue;
                }
                if i < sent.len() {
                    sent[i] = true;
                }
            } else if i < sent.len() {
                if !sent[i] {
                    continue;
                }
                sent[i] = false;
            }
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

// Maximizing, which on an infinite canvas has to be given a meaning rather than inherited from
// one. There is no screen for a window to fill: what it fills is whatever canvas rectangle the
// scripts name (the visible part of the canvas, by default), taken once when asked and fixed in
// canvas space afterwards. This is the mechanics: remember what unmaximizing gives back, stop
// anything animating the window, and tell the client its new size.
#[allow(clippy::too_many_arguments)]
fn maximize_window(
    windows: &mut Windows,
    state: &State,
    settling: &mut Vec<(WlSurface, f32, f32, f32)>,
    resize_settle: &mut Option<ResizeSettle>,
    surf: &WlSurface,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
) {
    // Read before anything moves: this is what unmaximizing gives back.
    if let (Some((ox, oy)), Some((ow, oh))) =
        (render::window_origin(windows, surf), render::geo_size(windows, surf))
    {
        render::maximize(windows, surf, x, y, ox, oy, ow, oh);
    }
    // Same housekeeping as a client's own move: an animation still running would drag the
    // window back off the shape we are about to give it.
    settling.retain(|(s, ..)| s != surf);
    if resize_settle.as_ref().map(|st| &st.surface) == Some(surf) {
        *resize_settle = None;
    }
    render::clear_scale(windows, surf);
    render::settle(windows, surf);
    render::front(windows, surf);
    render::set_window_origin(windows, surf, x, y);
    wl::state::maximize_toplevel(state, surf, w.round() as i32, h.round() as i32, true);
}

// And back to the rectangle it had. A window we never maximized still gets an answer, at the
// size it already is, because a client waiting on a configure it asked for is stuck until it
// arrives.
fn unmaximize_window(windows: &mut Windows, state: &State, surf: &WlSurface) {
    let restore = render::unmaximize(windows, surf);
    if let Some((x, y, _, _)) = restore {
        render::set_window_origin(windows, surf, x, y);
    }
    let (w, h) = restore
        .map(|(_, _, w, h)| (w, h))
        .or_else(|| render::geo_size(windows, surf))
        .unwrap_or((0.0, 0.0));
    wl::state::maximize_toplevel(state, surf, w.round() as i32, h.round() as i32, false);
}

// Dragging a window that is filling the view takes it out of maximized first.
//
// Only when it is actually filling it. Maximized here is a size, not a mode that owns a screen,
// and the window keeps it while you pan, zoom or move it, so a window that is still marked
// maximized may be sitting in the corner of the view at a third of the size. Dragging that one
// is just dragging it. The one worth taking apart is the one where a drag would otherwise be
// meaningless, because there is nowhere for it to go that looks any different.
//
// It comes back under your cursor rather than where it used to be: the remembered position is
// dropped and the remembered size is kept, with the grabbed point held at the same place in the
// window it was grabbed at. Grab the middle of a full-view window and you are still holding the
// middle of the small one, which is the mapping that keeps the window under your hand while it
// changes size.
fn unmaximize_for_drag(
    windows: &mut render::Windows,
    state: &State,
    surface: &WlSurface,
    cam3d: ray::Camera3D,
    // Where the pointer grabbed, in canvas units.
    gx: f32,
    gy: f32,
) -> bool {
    if !render::is_maximized(windows, surface) {
        return false;
    }
    let sw = ray::screen_width() as f32;
    let sh = ray::screen_height() as f32;
    let corners = (
        camera::screen_to_plane(cam3d, 0.0, 0.0, 0.0),
        camera::screen_to_plane(cam3d, sw, sh, 0.0),
    );
    let (Some((lx, ty)), Some((rx, by))) = corners else {
        return false;
    };
    if !render::covers(windows, surface, lx, ty, rx - lx, by - ty) {
        return false;
    }
    let (Some((x, y)), Some((w, h))) =
        (render::window_origin(windows, surface), render::geo_size(windows, surface))
    else {
        return false;
    };
    let Some((_, _, rw, rh)) = render::unmaximize(windows, surface) else {
        return false;
    };
    // Where in the window the grab was, as a fraction of it, so the same point is under the
    // cursor once it is small again.
    let fx = if w > 0.0 { (gx - x) / w } else { 0.5 };
    let fy = if h > 0.0 { (gy - y) / h } else { 0.5 };
    render::set_window_origin(windows, surface, gx - fx * rw, gy - fy * rh);
    wl::state::maximize_toplevel(state, surface, rw.round() as i32, rh.round() as i32, false);
    true
}

//
// Entry
//

fn main() {
    // Smithay and wayland-server report everything through tracing, including
    // client protocol errors, and without a subscriber all of it is dropped. That
    // is why a client dying used to leave no trace at all. YUKA_LOG overrides the
    // filter (e.g. YUKA_LOG=debug, or YUKA_LOG=smithay=trace).
    let filter = std::env::var("YUKA_LOG").unwrap_or_else(|_| "info".to_string());
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
        .with_writer(std::io::stderr)
        .init();

    // Behaviour lives in the scripts, not in the binary: what the keys do, where a scroll
    // goes, and the settings (lua/config.lua). Re-read while running (see
    // SETTINGS_POLL_FRAMES), so tuning is a save from another tty rather than a rebuild.
    // No scripts, no yuka: without them nothing answers input, so scripts that will not load
    // end the run here, before anything has taken the display or the keyboard.
    let lua_dir = std::env::var("YUKA_LUA").unwrap_or_else(|_| lua::DEFAULT_DIR.to_string());
    let (mut script, mut set) = match lua::init(&lua_dir) {
        Ok(loaded) => loaded,
        Err(e) => {
            eprintln!("yuka: lua: {e}");
            eprintln!("yuka: the scripts did not load, exiting");
            std::process::exit(1);
        }
    };
    // Commands the scripts asked for this frame, drained as they are applied.
    let mut cmds: Vec<lua::Cmd> = Vec::with_capacity(32);

    // Nested build: yuka is a client of another compositor, which owns the display
    // and the input devices. Everything that reaches for hardware stays out of it, so
    // there is no session to control, no DRM master to hold, no evdev to read, and
    // nothing that can lock the machine. Same source, one flag apart.
    let windowed = cfg!(feature = "windowed");

    // Session control. With it, logind owns our VT (K_OFF, KD_GRAPHICS) so the
    // switch chords are ours to implement and input needs no grab.
    //
    // Taking control makes us the only thing that can switch VTs, so we refuse it
    // when the machine has no keyboard to switch with: silencing the console then
    // would leave no way off this VT at all. YUKA_NO_SEAT opts out by hand, which
    // is also what non-interactive runs want.
    let no_seat = std::env::var("YUKA_NO_SEAT").is_ok();
    let mut seat = if windowed {
        println!("yuka: nested in another compositor: no session control, no vt switching");
        None
    } else if input::any_keyboard_present() && !no_seat {
        seat::init()
    } else {
        if !input::any_keyboard_present() {
            eprintln!(
                "yuka: no keyboard on this machine, so not taking session control: \
                 the console keeps its own ctrl+alt+Fn"
            );
        }
        None
    };

    // The card, before the window: logind hands out KMS devices, and raylib takes the
    // fd from us rather than opening one itself. Without the raylib patch there is
    // nothing to hand it to, so raylib opens the card and we adopt its fd afterwards.
    // YUKA_CARD picks the opener by hand: logind (the default) or raylib, which is
    // also what a raylib without the patch gets. It keeps that older path tested, and
    // is the first thing to try if the handoff ever misbehaves.
    let mut handed_card = false;
    let handoff = match std::env::var("YUKA_CARD").unwrap_or_default().as_str() {
        "" | "logind" => ray::can_set_drm_fd(),
        "raylib" => false,
        other => {
            eprintln!("yuka: YUKA_CARD={other} is neither logind nor raylib, using logind");
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
        ray::init_window(WINDOWED_W, WINDOWED_H, "yuka (nested)");
    } else {
        // 0, 0 lets raylib take the connector's current mode: the whole screen. A
        // virtual GPU whose current mode is not a clean match (virtio-gpu in a VM)
        // can land on a tiny fallback, so YUKA_SIZE=WxH names the mode by hand and
        // raylib picks that exact connector mode instead.
        let (w, h) = std::env::var("YUKA_SIZE")
            .ok()
            .and_then(|s| {
                let (a, b) = s.split_once('x')?;
                Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
            })
            .unwrap_or((0, 0));
        ray::init_window(w, h, "yuka");
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
    // time, so a second yuka is refused at open and ends up on the path below.
    let logind_card = seat.as_ref().map(seat::card_from_logind).unwrap_or(false);
    let display_ours = logind_card || seat::drm_is_ours();
    if ray::screen_width() <= 0 || ray::screen_height() <= 0 || !display_ours {
        eprintln!(
            "yuka: no usable display ({}x{}), exiting before taking any input.",
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
        "yuka: screen {}x{}  egl_display={:p}  egl_context={:p}",
        ray::screen_width(),
        ray::screen_height(),
        ray::egl_current_display(),
        ray::egl_current_context(),
    );

    // A label under every window saying what it is made of and how it is sampled.
    // YUKA_DEBUG_LABELS=1, or Super+I at any time.
    let mut debug_labels = std::env::var("YUKA_DEBUG_LABELS").is_ok();
    // The trackpad instrument in the corner: fingers, button regions, live gesture
    // state. YUKA_DEBUG_PAD=1, or Super+P at any time.
    let mut debug_pad = std::env::var("YUKA_DEBUG_PAD").is_ok();

    let egl = egl::init().expect("egl init");
    // How much anisotropic filtering the driver offers, asked once. It only matters
    // for minified windows, which is also the only time we build the mip chain it
    // needs, and the perspective tilt is what makes it worth having at all.
    let anisotropy = egl::max_anisotropy();
    let dmabuf_formats = egl.query_formats();
    let render_node_dev = egl.render_node_dev();
    println!(
        "yuka: egl reports {} dmabuf format/modifier pairs, anisotropy {anisotropy:.0}x",
        dmabuf_formats.len()
    );

    let shader = ray::load_shader("shaders/window.vert", "shaders/window.frag");
    let alpha_loc = ray::shader_location(shader, "alphaBlend");
    let swizzle_loc = ray::shader_location(shader, "swizzleBgra");
    let fade_loc = ray::shader_location(shader, "windowAlpha");

    let (mut server, mut state) =
        wl::state::init(
            dmabuf_formats,
            render_node_dev,
            ray::screen_width(),
            ray::screen_height(),
        )
        .expect("wayland init");
    println!("yuka: WAYLAND_DISPLAY={}", server.socket_name);

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
    // libinput would read the real devices even nested, so the host and yuka would
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
            "yuka: libinput opened no input devices, so nothing could quit or switch \
             away from yuka. Exiting instead of locking the seat. This is usually \
             permissions on /dev/input: try 'sg input -c ./target/debug/yuka'."
        );
        if let Some(s) = seat.as_mut() {
            seat::shutdown(s);
        }
        ray::close_window();
        return;
    }

    if seat.is_none() && !windowed {
        println!(
            "yuka: no session control: input grabbed, no vt switching, \
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
    // A window being moved, if any.
    let mut drag: Option<Drag> = None;
    // Active Super+right-drag resize, if any, and one released and waiting to be answered.
    let mut resize: Option<Resize> = None;
    let mut resize_settle: Option<ResizeSettle> = None;
    // Windows easing back down after a drop. Each entry is the surface plus its
    // world center and z at release; we hold the visual (screen) center fixed as
    // z returns to 0 so perspective does not slide it toward the screen center.
    let mut settling: Vec<(WlSurface, f32, f32, f32)> = Vec::new();

    // Programs the scripts started, killed on the way out.
    let mut children: Vec<Child> = Vec::new();
    // What the scripts see of the canvas each frame, and the window events since the last
    // one. Reused, so a frame allocates nothing for them.
    let mut win_list = render::win_list_new();
    let mut mapped_ids: Vec<(u64, bool)> = Vec::with_capacity(8);
    let mut activated_ids: Vec<u64> = Vec::with_capacity(8);
    let mut max_requests: Vec<(u64, bool)> = Vec::with_capacity(4);
    // Which of this frame's maximize requests the scripts answered.
    let mut max_answered: Vec<u64> = Vec::with_capacity(4);

    let start = Instant::now();
    // Behind the canvas, until the scripts say otherwise (yk.set_background).
    let mut clear = ray::Color { r: 24, g: 24, b: 32, a: 255 };
    let mut frame: u32 = 0;
    let debug_input = std::env::var("YUKA_DEBUG_INPUT").is_ok();
    // Dumps each toplevel's surface tree, which is what decides how children stack. Twice now
    // a child has drawn on the wrong side of something and the slots were the only way to see
    // what the client had actually asked for.
    let debug_tree = std::env::var("YUKA_DEBUG_TREE").is_ok();
    let shot_env = std::env::var("YUKA_SHOT");
    let screenshot = shot_env.is_ok();
    let shot_frame: u32 = shot_env
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(SHOT_FRAME);
    let max_frames: u32 = std::env::var("YUKA_MAX_FRAMES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(u32::MAX);
    let mut last = Instant::now();
    let mut max_dt_ms: f64 = 0.0;
    let mut slow_frames: u32 = 0;
    // Last session state we acted on, to catch activation edges from libseat.
    let mut session_active = true;
    // What the cursor should be showing, as of the last frame, for the debug trace only.
    let mut last_cursor_want = "arrow";
    // The dmabuf mode we last said out loud, so a live reload that changes it says so once.
    let mut announced_dmabuf: Option<DmabufMode> = None;
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
    let mut cursor_images: Vec<CursorImage> = Vec::new();
    // Cursors a client asked for by name, looked up in the theme once each. The name is kept
    // alongside a None for a shape the theme does not have, so a miss is not looked up again every
    // frame the pointer sits over the thing that wanted it.
    let mut named_cursors: Vec<(&'static str, Option<xcursor::Image>)> = Vec::new();
    // What we last handed the plane: surface and hotspot, so an unchanged cursor is not
    // rebuilt every frame.
    let mut cursor_key: Option<(ObjectId, i32, i32)> = None;
    // Cursor surfaces that committed this frame are handled before the plane is told anything,
    // so an attach and the set_cursor that follows it in the same batch resolve in that order.
    let mut cursor_moved: Vec<(ObjectId, i32, i32)> = Vec::new();
    // The dragged window's geometry as last seen, to notice the client changing it.
    let mut drag_geo: Option<(f32, f32, f32, f32)> = None;
    // Which keycodes a client has been told are down, so a release always follows its press.
    let mut keys_sent = [false; input::KEY_CODES];
    // Whether Super+S was pressed this frame, and how many shots this run has taken. The
    // number goes in the filename so a second one does not quietly replace the first.
    let mut shot_now = false;
    let mut shots: u32 = 0;
    // A pinch sequence in progress toward a client, and how far it has scaled since it began.
    // The protocol reports the total rather than the step, and a sequence outlives a frame.
    let mut pinch_active = false;
    let mut pinch_scale: f64 = 1.0;
    // And the finger scroll in progress toward a client, which outlives a frame for the same
    // reason: which axes it is sending, and how far each has travelled lately.
    let mut scroll_gesture = ScrollGesture {
        lock: ScrollLock::None,
        travel_x: 0.0,
        travel_y: 0.0,
        opened: false,
    };
    // Which of the two scales the zoom last settled at, which is the mode. Out here the canvas
    // owns the pointer and no window hears anything; in there they are applications again.
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
                    println!("yuka: back on vt{}", seat::our_vt(s));
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
                    println!("yuka: session inactive, display released");
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
        if frame > 5 {
            if dt_ms > max_dt_ms {
                max_dt_ms = dt_ms;
            }
            if dt_ms > 20.0 {
                slow_frames += 1;
            }
        }

        wl::state::accept_and_dispatch(&mut server, &mut state);
        // Retire popups the client has destroyed, and the grabs they were holding. Nothing
        // else does it: a dead popup stays in its grab's active list, the grab therefore
        // never reports itself ended, and a keyboard grab that never ends swallows every
        // focus change we make for the rest of the session.
        state.popups.cleanup();

        for key in state.dead_dmabufs.drain(..).collect::<Vec<_>>() {
            render::evict_dmabuf(&mut windows, &mut dmabuf_cache, &egl, &key);
        }
        wl::state::prune_held(&mut state);

        // A window that has gone takes the focus with it.
        //
        // Closing one is a click, and a click focuses what it lands on, so the window spends
        // its last moment focused and then stops existing. Nothing noticed: focused went on
        // holding a surface that was no longer there, which is not the same as holding
        // nothing. Two fingers went on believing they belonged to a window that had closed,
        // and the canvas would not take them until something else was clicked.
        //
        // Straight after the dispatch that carried the destroy, so nothing downstream sees a
        // frame of it.
        if focused.as_ref().is_some_and(|s| !s.is_alive()) {
            focus_window(&mut state, &mut focused, None);
        }

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
            // Where the client moved the buffer inside the surface, this commit. Recorded
            // whether or not new pixels came with it, and applied below once the surface is
            // known to be the cursor's.
            let (dx, dy) = wl::state::cursor_attach_delta(surface);
            // Once per surface per frame, whatever the number of commits. Smithay's current
            // state holds only the last attach, so a surface that committed twice offers the
            // same delta twice and counting both walks the hotspot out of the image.
            if (dx != 0 || dy != 0) && !cursor_moved.iter().any(|(k, ..)| *k == id) {
                cursor_moved.push((id.clone(), dx, dy));
            }
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
                    println!("yuka: cursor image {w}x{h} cached");
                }
                if let Some(slot) = cursor_images.iter_mut().find(|c| c.id == id) {
                    // New pixels for a surface we know: its hotspot bookkeeping carries over,
                    // because the surface is the same surface and the attaches accumulate.
                    slot.w = w;
                    slot.h = h;
                    slot.pixels = pixels;
                } else {
                    // A client has a handful of shapes; a cap keeps a misbehaving one from
                    // growing this without bound.
                    if cursor_images.len() >= 32 {
                        cursor_images.remove(0);
                    }
                    cursor_images.push(CursorImage {
                        id,
                        w,
                        h,
                        pixels,
                        hot: (0, 0),
                        set: u32::MAX,
                    });
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

        // Windows that have just drawn their first frame, for the scripts to decide about: whether
        // one takes the keyboard, whether the view goes to it. Each comes with whether it is from
        // the client that already has the keyboard, or nothing had it, which is the difference
        // between a dialog opening in front of you and a background application helping itself
        // to your keystrokes.
        mapped_ids.clear();
        for surface in windows.mapped.drain(..).collect::<Vec<_>>() {
            let welcome = match focused.as_ref() {
                Some(current) => same_client(current, &surface),
                None => true,
            };
            if let Some(id) = render::window_id(&windows, &surface) {
                mapped_ids.push((id, welcome));
            }
        }

        if debug_tree && frame % 120 == 0 {
            for surface in wl::state::toplevel_surfaces(&state) {
                let tree = wl::state::surface_tree(&surface);
                let root_slot = tree.iter().position(|(s, _, _)| *s == surface).unwrap_or(0);
                println!("yuka: tree {} surfaces, root at slot {root_slot}", tree.len());
                for (slot, (s, ox, oy)) in tree.iter().enumerate() {
                    let size = render::geo_size(&windows, s)
                        .map(|(w, h)| format!("{w:.0}x{h:.0}"))
                        .unwrap_or_else(|| "no entry".to_string());
                    println!(
                        "yuka:   slot {slot} off {ox:.0},{oy:.0} {size}{}",
                        if slot == root_slot { "  <- root" } else { "" }
                    );
                }
            }
        }

        // Windows another client asked us to activate: you clicked a link in one application and
        // a different one opened the page. The token is what makes it safe to honour, and whether
        // to trust it was decided at the protocol boundary; by the time it arrives here it is a
        // window the user asked for. What to do about it is the scripts'.
        activated_ids.clear();
        for surface in state.activation_requests.drain(..).collect::<Vec<_>>() {
            if !surface.is_alive() {
                continue;
            }
            if let Some(id) = render::window_id(&windows, &surface) {
                activated_ids.push(id);
            }
        }

        // Clients asking to be maximized or let go of it, for the scripts to answer.
        //
        // Never at map time, which is decided here: a client that asks to be maximized before it
        // has appeared is asking to open filling the screen, and a window that seizes the view on
        // launch moves you somewhere you did not go. Refused rather than ignored, because a
        // client that asked is owed a configure and sits waiting for one otherwise. It gets 0x0,
        // the protocol's "your size is your own business".
        max_requests.clear();
        for (surf, on) in state.maximize_requests.drain(..).collect::<Vec<_>>() {
            if on && render::geo_size(&windows, &surf).is_none() {
                wl::state::decline_maximize(&state, &surf);
                continue;
            }
            if let Some(id) = render::window_id(&windows, &surf) {
                max_requests.push((id, on));
            }
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
            input::poll(i, ray::screen_width(), ray::screen_height());
        }

        // Pick up edited scripts. Every setting is read where it is used, per event or per
        // frame, so a save lands on the next frame with no further work.
        //
        // Reloading on demand is the scripts' own binding (Super+R by default).
        if frame % SETTINGS_POLL_FRAMES == 0 {
            if let Some(new) = lua::reload_if_changed(&mut script) {
                adopt_settings(&mut set, new);
            }
        }

        // Say which way dmabufs are being handled, at startup and on every reload that
        // changes it.
        if announced_dmabuf != Some(set.dmabuf_mode) {
            announced_dmabuf = Some(set.dmabuf_mode);
            match set.dmabuf_mode {
                DmabufMode::Hold => println!("yuka: dmabuf_mode hold, buffers sampled in place"),
                DmabufMode::Blit => println!(
                    "yuka: dmabuf_mode blit, buffers copied into textures we own and released at once"
                ),
            }
        }

        // Quit chord (ctrl+alt+backspace, the traditional one). While we hold the
        // keyboard grab, ctrl+c in the shell that launched us cannot reach it, so
        // without this there is no way to stop yuka from the keyboard at all.
        if let Some(i) = inp.as_ref() {
            let quit = input::ctrl_down(i)
                && input::alt_down(i)
                && input::events(i).iter().any(|&(c, p)| p && c == input::KEY_BACKSPACE);
            if quit {
                println!("yuka: ctrl+alt+backspace, quitting");
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
                println!("yuka: switching to vt{target}");
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
        // decided by the scripts, alongside the wheel's, so a trackpad can scroll a window.
        let pad = match touchpad.as_mut() {
            Some(tp) => touch::update(tp, cursor.as_mut(), &set),
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
        // What lets go of every window, which is the scripts' to ask for (Super+Escape and the
        // three-finger tap, by default).
        let mut release_all = false;

        // And its scroll joins the mouse's, in the same fields libinput fills when it
        // drives the trackpad itself. Everything downstream then treats the two modes
        // identically, including forwarding to whatever the pointer is over.
        ptr.scroll_x += pad.scroll_x;
        ptr.scroll_y += pad.scroll_y;
        ptr.pinch *= pad.pinch;
        let scroll_ended = pad.scroll_ended || ptr.scroll_ended;
        let pad_driving = set.trackpad_mode == input::TrackpadMode::Custom;
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
        pressed |= ptr.left_pressed;
        released |= ptr.left_released;

        // The scripts' turn. They see the frame and say what should happen: bindings, gestures,
        // where the wheel, the fingers and a pinch go, how the view moves, which window has the
        // keyboard. Applied here, in the order asked, before anything below reads the camera.
        let (sw, sh) = (ray::screen_width() as f32, ray::screen_height() as f32);
        render::list_windows(&windows, &mut win_list);
        // The window under the pointer, by the view as it stands before the scripts move it.
        let under = pointer_xy.and_then(|(x, y)| {
            let cam3d = camera::camera_3d(&cam, ray::screen_width(), ray::screen_height(), &set);
            render::window_at(&windows, cam3d, x as f32, y as f32)
                .map(|(surf, ..)| wl::state::window_root(&state, &surf))
                .and_then(|root| render::window_id(&windows, &root))
        });
        let no_keys = [false; input::KEY_CODES];
        let route = {
            let f = lua::FrameIn {
                keys: inp.as_ref().map(input::events).unwrap_or(&[]),
                held: inp.as_ref().map(input::keys).unwrap_or(&no_keys),
                super_down,
                ctrl_down: inp.as_ref().map(input::ctrl_down).unwrap_or(false),
                alt_down: inp.as_ref().map(input::alt_down).unwrap_or(false),
                shift_down: inp.as_ref().map(input::shift_down).unwrap_or(false),
                pointer: pointer_xy.map(|(x, y)| (x as f32, y as f32)),
                dx: ptr.dx,
                dy: ptr.dy,
                pressed,
                middle_pressed: ptr.middle_pressed,
                wheel: ptr.wheel,
                hwheel: ptr.hwheel,
                middle: ptr.middle,
                scroll_x: ptr.scroll_x,
                scroll_y: ptr.scroll_y,
                scroll_lifted: ptr.scroll_ended,
                pad_driving,
                fingers: pad.fingers as u32,
                pinch: ptr.pinch,
                swiping: pad.swiping,
                swipe_progress: pad.swipe_progress,
                swipe_dir: pad.swipe_dir,
                swipe_fired: pad.swipe_fired,
                tap3: pad.three_finger_double_tap,
                focused: focused.as_ref().and_then(|s| render::window_id(&windows, s)),
                // The window the hovered surface belongs to: the pointer may be over a
                // subsurface or a popup, and the scripts name windows.
                hovered: hovered
                    .as_ref()
                    .map(|s| wl::state::window_root(&state, s))
                    .and_then(|s| render::window_id(&windows, &s)),
                under,
                dragging: drag.is_some(),
                right_pressed: ptr.right_pressed,
                // Only our own resize: a client dragging its own edge named the edge.
                resizing: resize
                    .as_ref()
                    .filter(|r| r.right_button)
                    .and_then(|r| render::window_id(&windows, &r.surface).map(|id| (id, r.edge_x, r.edge_y))),
                zoom: cam.zoom,
                cx: cam.cx,
                cy: cam.cy,
                sw,
                sh,
                dt: ray::frame_time(),
                time_ms: start.elapsed().as_millis() as u32,
                frame,
                zoom_min: set.zoom_min,
                zoom_max: set.zoom_max,
                zoom_default: set.zoom_default,
                double_click_ms: set.double_click_ms,
                windows: &win_list,
                mapped: &mapped_ids,
                activated: &activated_ids,
                max_requests: &max_requests,
            };
            lua::frame(&mut script, &f, &mut cmds)
        };
        let mut reload_now = false;
        // Grabs the scripts started this frame, picked up by the move and resize blocks below.
        let mut begin_move: Option<(u64, bool)> = None;
        let mut begin_resize: Option<(u64, f32, f32)> = None;
        let mut resize_corner: Option<(f32, f32)> = None;
        render::clear_alpha_targets(&mut windows);
        max_answered.clear();
        for cmd in cmds.drain(..) {
            match cmd {
                lua::Cmd::Reload => reload_now = true,
                lua::Cmd::ToggleDebug(which) => {
                    let (flag, name) = match which {
                        lua::Debug::Labels => (&mut debug_labels, "debug labels"),
                        lua::Debug::Pad => (&mut debug_pad, "trackpad overlay"),
                    };
                    *flag = !*flag;
                    println!("yuka: {name} {}", if *flag { "on" } else { "off" });
                }
                // Asks. xdg_toplevel.close is a request, and a client may answer it with a dialog
                // about unsaved work or ignore it entirely. Killing the process instead would be
                // deciding on its behalf that whatever it was holding does not matter.
                lua::Cmd::Close(id) => {
                    if let Some(surf) = render::surface_by_id(&windows, id) {
                        wl::state::close_toplevel(&state, &surf);
                    }
                }
                // Taken at the end of the draw, so what lands in the file is the frame that was
                // on screen, cursor plane aside: that is a hardware overlay the GPU never
                // composites, so it cannot be in a readback.
                lua::Cmd::Screenshot => shot_now = true,
                lua::Cmd::SetCamera(cx, cy, zoom) => {
                    cam.cx = cx;
                    cam.cy = cy;
                    cam.zoom = zoom.clamp(set.zoom_min, set.zoom_max);
                }
                // Keyboard focus follows the toplevel, never a popup or a subsurface.
                lua::Cmd::Focus(id) => {
                    let next = id
                        .and_then(|id| render::surface_by_id(&windows, id))
                        .map(|s| wl::state::window_root(&state, &s));
                    focus_window(&mut state, &mut focused, next);
                }
                lua::Cmd::Front(id) => {
                    if let Some(surf) = render::surface_by_id(&windows, id) {
                        render::front(&mut windows, &surf);
                    }
                }
                lua::Cmd::EatPress => pressed = false,
                lua::Cmd::BeginMove(id, lift) => begin_move = Some((id, lift)),
                lua::Cmd::BeginResize(id, ex, ey) => begin_resize = Some((id, ex, ey)),
                lua::Cmd::ResizeCorner(ex, ey) => resize_corner = Some((ex, ey)),
                lua::Cmd::WindowAlpha(id, alpha) => {
                    if let Some(surf) = render::surface_by_id(&windows, id) {
                        render::set_alpha_target(&mut windows, &surf, alpha);
                    }
                }
                lua::Cmd::Maximize(id, x, y, w, h) => {
                    if let Some(surf) = render::surface_by_id(&windows, id) {
                        maximize_window(
                            &mut windows,
                            &state,
                            &mut settling,
                            &mut resize_settle,
                            &surf,
                            x,
                            y,
                            w,
                            h,
                        );
                        max_answered.push(id);
                    }
                }
                lua::Cmd::Unmaximize(id) => {
                    if let Some(surf) = render::surface_by_id(&windows, id) {
                        unmaximize_window(&mut windows, &state, &surf);
                        max_answered.push(id);
                    }
                }
                lua::Cmd::MoveWindow(id, x, y) => {
                    if let Some(surf) = render::surface_by_id(&windows, id) {
                        render::set_window_origin(&mut windows, &surf, x, y);
                    }
                }
                lua::Cmd::Background(rgba) => {
                    clear = ray::Color {
                        r: (rgba >> 24) as u8,
                        g: (rgba >> 16) as u8,
                        b: (rgba >> 8) as u8,
                        a: rgba as u8,
                    };
                }
                lua::Cmd::ReleaseAll => release_all = true,
                lua::Cmd::Spawn(cmd) => {
                    if let Some(child) = spawn_client(&server.socket_name, &cmd) {
                        children.push(child);
                    }
                }
            }
        }
        // A request the scripts let pass still gets its configure, since the client is waiting
        // on one: refused if it asked for maximized, its own size back if it asked to leave.
        for &(id, on) in max_requests.iter() {
            if max_answered.contains(&id) {
                continue;
            }
            if let Some(surf) = render::surface_by_id(&windows, id) {
                if on {
                    wl::state::decline_maximize(&state, &surf);
                } else {
                    unmaximize_window(&mut windows, &state, &surf);
                }
            }
        }
        if reload_now {
            if let Some(new) = lua::reload(&mut script) {
                adopt_settings(&mut set, new);
            }
        }
        // The stop goes even when the pointer has wandered off the window, because a client
        // left holding an open sequence is the thing being fixed.
        let forward_wheel = route.wheel;
        let scroll_to_client = route.fingers;

        render::prune_dead(&mut windows);
        render::animate(&mut windows, ray::frame_time());
        // The view lands on whole pixels whatever moved it: keys, wheel, middle drag,
        // trackpad, or a zoom reset that changed the scale without touching the pan. Applied
        // as the 3D camera is built rather than written back into cam, so no code path can
        // forget it and none can accumulate it either.
        let cam3d = camera::camera_3d(&cam, ray::screen_width(), ray::screen_height(), &set);

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
        // drag is a resize request.
        //
        // Drained here, ahead of the two blocks that act on it, because a request that
        // arrives at the top of this frame and is only picked up below them does not reach
        // the quad until the next frame. That was a whole frame of the gap between grabbing
        // a titlebar and the window starting to follow, on top of the round trip we cannot
        // avoid: the press has to reach the client and its request has to come back.
        let asked_move: Vec<(WlSurface, u32)> = state.move_requests.drain(..).collect();
        let asked_resize: Vec<(WlSurface, i32, i32, u32)> = state.resize_requests.drain(..).collect();
        {
            if let Some((surf, serial)) = asked_move.into_iter().last() {
                // Anchor to the press the client is quoting. Between that press and this
                // request sits the client's own latency, and a pointer that kept moving
                // through it: anchoring to where the pointer is now would drag the window
                // from a point you never grabbed. Falls back to now if the serial is stale.
                let (ax, ay, af) = press_at(&press_log, serial, sxp, syp, frame);
                if debug_input {
                    let (w, h) = render::geo_size(&windows, &surf).unwrap_or((0.0, 0.0));
                    println!(
                        "yuka: move request {}x{} {} took {} frames (press frame {af}, now {frame}), pointer travelled {:.0},{:.0} since",
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
                    unmaximize_for_drag(&mut windows, &state, &surf, cam3d, gx, gy);
                    if let Some((ox, oy)) = render::surface_origin(&windows, &surf) {
                        settling.retain(|(s, ..)| s != &surf);
                        if resize_settle.as_ref().map(|st| &st.surface) == Some(&surf) {
                            resize_settle = None;
                        }
                        render::clear_scale(&mut windows, &surf);
                        // Neither lifted nor raised. The client asked to be moved, not picked up,
                        // and a drag from inside a window is the same kind of act as a click in
                        // one: it says where the window goes, not what is in front of what. The
                        // lift belongs to a Super+drag, where you did take hold of it, and the
                        // stack should follow the same rule as the animation rather than one each.
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
                    // The declared limits come with it, because they are what the quad is
                    // now held to and a window that declares nothing is a different case to
                    // read about than one that declares and is being obeyed.
                    let (lo, hi) = wl::state::size_limits(&surf);
                    println!(
                        "yuka: resize request {}x{} {} took {} frames (press frame {af}, now {frame}), pointer travelled {:.0},{:.0} since, client says min {}x{} max {}x{}",
                        w as i32,
                        h as i32,
                        render::source(&windows, &surf),
                        frame.saturating_sub(af),
                        sxp - ax,
                        syp - ay,
                        lo.0, lo.1, hi.0, hi.1
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
        // Behind Super, always, which is the compositor's way of moving and resizing a window
        // as opposed to the client's own titlebar and edges.
        //
        // The overview used to let a drag do it with nothing held, on the grounds that no
        // client was listening out there so a drag could not mean anything else. That was a
        // mode you could be in without being told: a plain drag moved a window or worked in
        // it depending on a gesture made minutes earlier, with nothing on screen to say
        // which. One way to pick a window up, and it is the same everywhere.
        //
        // When one starts, on which window and holding which corner is the scripts' call
        // (yk.begin_resize); from here on it is mechanics.
        if let (Some((id, edge_x, edge_y)), None) = (begin_resize.take(), resize.as_ref()) {
            let root = render::surface_by_id(&windows, id);
            if let Some(root) = root {
                if let Some((gx, gy)) = camera::screen_to_plane(cam3d, sxp, syp, 0.0) {
                    if let Some((w, h)) = render::geo_size(&windows, &root) {
                        // A window still settling from a previous resize starts fresh.
                        if resize_settle.as_ref().map(|s| &s.surface) == Some(&root) {
                            resize_settle = None;
                        }
                        render::clear_scale(&mut windows, &root);
                        let (ox, oy) = render::window_origin(&windows, &root).unwrap_or((0.0, 0.0));
                        if debug_input {
                            println!(
                                "yuka: resize grabbed {:.0},{:.0} into {w:.0}x{h:.0}, holding the {} corner",
                                gx - ox,
                                gy - oy,
                                match (edge_x < 0.0, edge_y < 0.0) {
                                    (true, true) => "top left",
                                    (false, true) => "top right",
                                    (true, false) => "bottom left",
                                    (false, false) => "bottom right",
                                }
                            );
                        }
                        resize = Some(Resize {
                            surface: root,
                            grab_x: gx,
                            grab_y: gy,
                            from_x: ox,
                            from_y: oy,
                            from_w: w,
                            from_h: h,
                            edge_x,
                            edge_y,
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
            // Which corner the drag holds can change without letting go: the scripts re-test it
            // every frame (yk.resize_corner) and this re-seeds the drag when they move it.
            //
            // Only for our own drag. A client dragging its own edge named which edge that was,
            // and a request to move the right edge must not turn into moving the top one.
            let corner = resize_corner.take().filter(|_| alive && r.right_button);
            if let Some((ex, ey)) = corner {
                if let (Some((cx, cy)), Some((ow, oh)), Some((ox, oy))) = (
                    camera::screen_to_plane(cam3d, sxp, syp, 0.0),
                    render::geo_size(&windows, &r.surface),
                    render::window_origin(&windows, &r.surface),
                ) {
                    if ex != r.edge_x || ey != r.edge_y {
                        if debug_input {
                            println!(
                                "yuka: resize now holding the {} corner",
                                match (ex < 0.0, ey < 0.0) {
                                    (true, true) => "top left",
                                    (false, true) => "top right",
                                    (true, false) => "bottom left",
                                    (false, false) => "bottom right",
                                }
                            );
                        }
                        // Re-seeded from here, because the size is measured as displacement
                        // from where the drag was grabbed. Without this the new corner would
                        // inherit everything the old one had accumulated and the window would
                        // jump by that much on the frame it changed hands.
                        //
                        // What the client can manage is not re-seeded: the limits it has shown
                        // us are a property of the window, not of which corner we are holding.
                        r.edge_x = ex;
                        r.edge_y = ey;
                        r.grab_x = cx;
                        r.grab_y = cy;
                        r.from_x = ox;
                        r.from_y = oy;
                        r.from_w = ow;
                        r.from_h = oh;
                        r.asked = (ow, oh);
                        r.seen = (ow, oh);
                        r.waited = 0;
                    }
                }
            }
            // Canvas units are surface pixels at zoom 1, and projecting the cursor
            // already divides by the zoom, so the corner tracks the cursor at any zoom.
            // Where the cursor says the corner is, held back at whatever the client has
            // shown it cannot pass. Written as min-then-max rather than clamp: bounds
            // learned at runtime can cross, and clamp panics when they do.
            // What the window says it can be, read fresh because a client may change its mind
            // mid-drag. Zero in either direction is Wayland for "no limit", and for a minimum
            // that is where our own floor stands in, since a window that never said has to be
            // stopped from being dragged away to nothing.
            //
            // These are the same numbers resize_toplevel clamps the ask to, which is the whole
            // point: the quad cannot show a size the client was never going to be given. It
            // used to clamp only to resize_min_px, so dragging under a declared minimum
            // stretched the window somewhere it could not go, and it sat there until the
            // learned limits noticed a few commits later and snapped it back. The window is
            // the source of truth about its own size, and it had already said so.
            let (said_min, said_max) = wl::state::size_limits(&r.surface);
            let min_w = if said_min.0 > 0 { said_min.0 as f32 } else { set.resize_min_px };
            let min_h = if said_min.1 > 0 { said_min.1 as f32 } else { set.resize_min_px };
            let max_w = if said_max.0 > 0 { said_max.0 as f32 } else { f32::MAX };
            let max_h = if said_max.1 > 0 { said_max.1 as f32 } else { f32::MAX };
            // Behaviour on top of declaration, never under it: a client that declares nothing
            // and simply refuses is still learned from, and one that declares is held to what
            // it said even if it happens to have managed less.
            let floor_w = if r.stuck_small >= RESIZE_STUCK_COMMITS { r.least_w.max(min_w) } else { min_w };
            let floor_h = if r.stuck_small >= RESIZE_STUCK_COMMITS { r.least_h.max(min_h) } else { min_h };
            let ceil_w = if r.stuck_large >= RESIZE_STUCK_COMMITS { r.most_w.min(max_w) } else { max_w };
            let ceil_h = if r.stuck_large >= RESIZE_STUCK_COMMITS { r.most_h.min(max_h) } else { max_h };
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
                    // How big the window is on screen this frame: the size the cursor is
                    // asking for while the stretch is showing it, and otherwise simply what
                    // the client has committed.
                    let (shown_w, shown_h) = if set.resize_stretch { (w, h) } else { (gw, gh) };
                    // Dragging a near edge keeps the far one still, which means the window
                    // has to travel as it grows. Against the size on screen, not the size we
                    // asked for: with the stretch off those are different numbers for as long
                    // as the client takes to answer, and moving the origin by the ask while
                    // the quad stayed its old size slid the window and dragged the far edge
                    // off its mark.
                    if r.edge_x < 0.0 || r.edge_y < 0.0 {
                        let x = if r.edge_x < 0.0 { r.from_x + r.from_w - shown_w } else { r.from_x };
                        let y = if r.edge_y < 0.0 { r.from_y + r.from_h - shown_h } else { r.from_y };
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
                        wl::state::resize_toplevel(&state, &r.surface, w as i32, h as i32, true);
                        r.asked = (w, h);
                        r.seen = (gw, gh);
                        r.waited = 0;
                    }
                }
                _ => {
                    // Released: ask once, for the size the drag ended on, and keep any
                    // stretch until the client answers so the window does not snap back to
                    // its old size for a frame. With no stretch there is nothing being held
                    // and nothing to wait for: the quad has been showing the client's own
                    // size all along, so the last ask simply lands whenever it lands.
                    if alive {
                        let was = render::geo_size(&windows, &r.surface)
                            .unwrap_or((r.from_w, r.from_h));
                        // Unconditional, where it used to be skipped if the cursor could not
                        // be projected: this configure is what takes the client back out of
                        // Resizing, and one that never goes out leaves it laying out for a
                        // drag that finished.
                        let (w, h) = want.unwrap_or(was);
                        wl::state::resize_toplevel(&state, &r.surface, w as i32, h as i32, false);
                        if set.resize_stretch {
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

        // Super+drag picks a window up. Whether, which, and whether it lifts toward the camera
        // are the scripts' (yk.begin_move); the press that started it is consumed, so it is not
        // also focused or forwarded.
        let grab = begin_move.take().filter(|_| drag.is_none());
        if let Some((id, lift)) = grab {
            let surf = render::surface_by_id(&windows, id);
            let origin = surf.as_ref().and_then(|s| render::surface_origin(&windows, s));
            if let (Some(surf), Some((ox, oy))) = (surf, origin) {
                // Re-grabbing a still-settling window: cancel its settle.
                settling.retain(|(s, ..)| s != &surf);
                // Offset from the window origin to the grabbed point, captured
                // on the z=0 plane (constant in world units, independent of z).
                let (gx, gy) = camera::screen_to_plane(cam3d, sxp, syp, 0.0)
                    .unwrap_or((ox, oy));
                // Before the offsets are taken, since this can change both the window's size
                // and where it is.
                unmaximize_for_drag(&mut windows, &state, &surf, cam3d, gx, gy);
                let (ox, oy) = render::surface_origin(&windows, &surf).unwrap_or((ox, oy));
                if lift {
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
                    println!("yuka: drag geometry now {geo:?} (was {drag_geo:?})");
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
                        "yuka: drag end from_client={from_client} zoom {:.3} cursor moved {:.1},{:.1} window moved {:.1},{:.1} (screen px) drift {:.1},{:.1}",
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

        // The cursor image belongs to whatever is under the pointer: a text field wants an
        // I-beam and a resize edge wants an arrow, and only the client knows which. A client
        // asking for no cursor gets none. Over empty canvas it is ours: the same arrow, so the
        // pointer does not change shape for having nothing under it.
        //
        // The hotspot comes with it, which is the part that was wrong before: a cursor
        // image says where in itself the pointer actually is, and without honouring that
        // the click lands somewhere other than the tip.
        // Hand the plane whatever the client is currently asking for, from the cache. The
        // hotspot is read live, because set_cursor can change it without the surface
        // committing anything.
        // Every attach from this frame first, then whatever set_cursor said, in that order:
        // a client that moves its buffer and then states a hotspot outright means the second.
        for (id, dx, dy) in cursor_moved.drain(..) {
            if let Some(img) = cursor_images.iter_mut().find(|c| c.id == id) {
                // Clamped into the image, because a pointer tip outside its own cursor is not a
                // thing a client can mean. Without this, a client that restates one hotspot
                // while its attaches keep moving the buffer walks the tip off the picture: seen
                // reaching 63,7 inside a 30x30 cursor, which is 33 pixels of nothing.
                img.hot.0 = (img.hot.0 - dx).clamp(0, img.w.max(1) - 1);
                img.hot.1 = (img.hot.1 - dy).clamp(0, img.h.max(1) - 1);
                cursor_key = None;
            }
        }
        if let (Some(cur), CursorImageStatus::Surface(surf)) = (cursor.as_mut(), &state.cursor_image)
        {
            let id = surf.id();
            let (hx, hy) = wl::state::cursor_hotspot(surf);
            if let Some(img) = cursor_images.iter_mut().find(|c| c.id == id) {
                // A set_cursor we have not acted on yet: the client is stating where the pointer is
                // in its surface, so that is the hotspot, and anything the attaches had subtracted
                // is done with.
                if img.set != state.cursor_serial {
                    img.set = state.cursor_serial;
                    img.hot = (hx, hy);
                    cursor_key = None;
                }
            }
            if let Some(img) = cursor_images.iter().find(|c| c.id == id) {
                let (w, h) = (img.w, img.h);
                let (ox, oy) = img.hot;
                if cursor_key.as_ref() != Some(&(id.clone(), ox, oy)) {
                    if debug_input {
                        println!(
                            "yuka: cursor now {w}x{h} hotspot {ox},{oy} (client said {hx},{hy})"
                        );
                    }
                    cursor::store_client_image(
                        cur,
                        w,
                        h,
                        w * 4,
                        img.pixels.as_ptr() as *const u8,
                        ox,
                        oy,
                    );
                    cursor_key = Some((id, ox, oy));
                }
            }
        }

        if let Some(cur) = cursor.as_mut() {
            let client_cursor = hovered.is_some();
            if debug_input {
                let want = match (client_cursor, &state.cursor_image) {
                    (true, CursorImageStatus::Surface(_)) => "client",
                    (true, CursorImageStatus::Hidden) => "hidden",
                    (true, CursorImageStatus::Named(_)) => "named",
                    _ => "arrow",
                };
                if want != last_cursor_want {
                    println!("yuka: cursor want {want} (was {last_cursor_want})");
                    last_cursor_want = want;
                }
            }
            match (client_cursor, &state.cursor_image) {
                (true, CursorImageStatus::Surface(_)) if cursor::has_client_image(cur) => {
                    cursor::apply_client(cur);
                }
                (true, CursorImageStatus::Hidden) => cursor::set_hidden(cur),
                // A shape asked for by name rather than sent as pixels: the client says "text" or
                // "grab" and looking it up in the theme is ours to do. Same theme the windows use,
                // so an I-beam here is the I-beam they would have drawn.
                //
                // Every name is tried in turn, since themes disagree about what to call things:
                // the protocol's own name first, then the older X11 ones it is also known by.
                (true, CursorImageStatus::Named(icon)) => {
                    let name = icon.name();
                    let at = match named_cursors.iter().position(|(n, _)| *n == name) {
                        Some(at) => at,
                        None => {
                            let mut found = xcursor::load(name, 0);
                            for alt in icon.alt_names() {
                                if found.is_some() {
                                    break;
                                }
                                found = xcursor::load(alt, 0);
                            }
                            if found.is_none() && debug_input {
                                println!("yuka: no theme cursor for {name}, using our own");
                            }
                            named_cursors.push((name, found));
                            named_cursors.len() - 1
                        }
                    };
                    match named_cursors[at].1.as_ref() {
                        Some(image) => cursor::set_named(cur, at as u32, image),
                        None => cursor::set_arrow(cur),
                    }
                }
                // A surface whose pixels we have not seen yet, and empty canvas.
                _ => cursor::set_arrow(cur),
            }
        }

        // Crossing out into the overview lets go: a window still holding the keyboard from up
        // close would go on taking keys meant for the canvas, and one holding an implicit grab
        // would go on receiving a drag it can no longer see.
 
        // Menus and subsurfaces are anchored to their root, so they are placed after
        // everything that could have moved that root: the drag, the settle, the client's own
        // move and resize requests.
        //
        // This used to run before all of that, which meant a window's children were placed
        // from where it was a frame ago. Invisible on a client without subsurfaces, and
        // visible on Chromium as part of the window trailing the rest of it while dragging.
        // Maximized windows keep their pinned top-left first, so children placed below and
        // the pixel alignment after both see the corrected position.
        // Each window's opacity toward what the scripts asked for this frame.
        render::ease_alpha(&mut windows, ray::frame_time().min(0.05));
        render::hold_maximized(&mut windows);
        render::sync_children(&mut windows);

        // Everything that could move a window has run. Put them all on the pixel grid, then
        // decide how each one is sampled from where it actually ended up.
        render::align_positions(&mut windows, cam.zoom);
        // Copy first, so a window that has just become ours is mipped on the same frame rather
        // than spending one frame minified without a chain.
        //
        // And only when a chain is wanted at all: the copy exists to make one possible, so with
        // mips off it would be a texture allocated, a blit run and a buffer doubled in memory for
        // nothing. Turning mips off is the way to spend nothing here on a machine that cannot
        // afford it, and it has to actually mean that.
        if set.mips_when_minified {
            render::blit_minified(&mut windows, &egl, cam.zoom, set.dmabuf_blit_below);
        }
        render::prepare_textures(&mut windows, cam.zoom, anisotropy, set.mips_when_minified);

        // Dismissal is the popup grab's job now, not ours: it knows the chain and
        // tells the client in the right order. YUKA_DEBUG_INPUT=1 still dumps the
        // click and every rect it could have hit.
        if debug_input && pressed {
            println!("yuka: click screen {sxp:.0},{syp:.0}");
            render::log_rects(&windows);
        }

        let time_ms = start.elapsed().as_millis() as u32;
        {
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
                debug_input,
                route.pointer,
                drag.as_ref().map(|d| d.from_client).unwrap_or(false)
                    || resize.as_ref().map(|r| !r.right_button).unwrap_or(false),
                &mut press_log,
                release_all,
                &ptr,
                pressed,
                released,
                time_ms,
                frame,
            );
            // The stop goes even when the pointer has wandered off the window, because a
            // client left holding an open sequence is the thing being fixed.
            if forward_wheel || scroll_to_client || scroll_ended {
                forward_scroll(
                    &mut state,
                    &ptr,
                    forward_wheel,
                    scroll_to_client,
                    scroll_ended,
                    &mut scroll_gesture,
                    time_ms,
                    &set,
                );
            }
            forward_pinch(
                &mut state,
                pad.zoom_started,
                pad.zoom_ended || ptr.pinch_ended,
                pad.pinch,
                route.pinch,
                &mut pinch_active,
                &mut pinch_scale,
                time_ms,
            );
        }
        // Outside the mode gate on purpose: xkb state has to follow the keyboard even when
        // no client is listening, or the next client to get focus inherits modifiers that
        // nobody is holding.
        if let Some(i) = inp.as_ref() {
            forward_keys(&mut state, i, route.keys, &mut keys_sent, time_ms);
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
        render::draw_windows(
            &windows,
            cam3d,
            shader,
            alpha_loc,
            swizzle_loc,
            fade_loc,
            set.draw_shadows,
        );
        if debug_labels {
            render::draw_debug_labels(&windows, cam3d, cam.zoom, anisotropy);
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
        // The scripts' own drawing, over the windows and the instruments.
        lua::draw(&mut script);
        // Scripts that would not load or run say so on screen, since the log may be on a tty
        // nobody is looking at.
        if let Some(err) = script.error.as_ref() {
            render::draw_script_error(err, ray::screen_width());
        }
        if screenshot && frame == shot_frame {
            ray::flush_batch();
            ray::take_screenshot("shot.png");
        }
        if shot_now {
            shot_now = false;
            shots += 1;
            let path = format!("shot-{shots}.png");
            // Everything queued has to be in the framebuffer before it is read back, or the
            // file is missing whatever the batch was still holding.
            ray::flush_batch();
            ray::take_screenshot(&path);
            println!("yuka: wrote {path}");
        }
        // Just before the present, so the plane and the frame land together.
        if let Some(cur) = cursor.as_mut() {
            cursor::present_deferred(cur);
        }
        ray::end_drawing();


        frame += 1;
    }

    println!(
        "yuka: shutting down after {frame} frames; cached dmabufs={} max_frame={:.1}ms slow_frames(>20ms)={}",
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
