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

// A journey the view is on, and the spring carrying it.
//
// The spring drives a progress from 0 to 1 rather than the zoom and the centre themselves, and
// both are read off that progress. Which is the whole reason it is built this way: one spring
// cannot fall out of step with itself, so the scale and the pan arrive together, and when it
// overshoots they overshoot together and come back together. Two springs, one per half, would
// have to be kept in agreement about where "past the end" is.
//
// `to` is a canvas position the centre travels to, which is what "bring me that window" means.
// Without it the journey turns around a fixed screen point, ax and ay, which is what the swipes
// and the 1:1 detent do: the scale changes and the view stays where it was pointed.
struct Journey {
    // The scale it left and the one it is going to. Read geometrically: scale is not perceived
    // linearly, so halfway along is the geometric mean rather than the arithmetic one.
    from_zoom: f32,
    target: f32,
    to: Option<(f32, f32)>,
    // How far the centre was from its destination when it started, in screen pixels. Screen
    // rather than canvas, because what you watch is the canvas distance times the zoom and the
    // zoom is moving: eased in canvas units the view barely moves at first and then slides at the
    // end, which is a pan and a zoom disagreeing about how far along they are.
    from_off: (f32, f32),
    ax: f32,
    ay: f32,
    // The spring itself: where it is between the two ends, and how fast. Progress can pass 1 and
    // come back, which is the overshoot.
    p: f32,
    v: f32,
    // Which pair of spring numbers carries it. A swipe wants a different character from a journey
    // to a window: the fingers already made the motion, so the spring is only finishing it, and
    // bounce there reads as the view arguing with the hand that just let go. Read per frame rather
    // than copied in, so both pairs stay live to a reload.
    swipe: bool,
}

// Start one from where the view is now.
fn journey_to(
    cam: &camera::Camera,
    target: f32,
    to: Option<(f32, f32)>,
    ax: f32,
    ay: f32,
    swipe: bool,
) -> Journey {
    let from_off = match to {
        Some((tx, ty)) => ((tx - cam.cx) * cam.zoom, (ty - cam.cy) * cam.zoom),
        None => (0.0, 0.0),
    };
    Journey { from_zoom: cam.zoom, target, to, from_off, ax, ay, p: 0.0, v: 0.0, swipe }
}

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
            println!("om_wm: spawned '{cmd}' (pid {})", child.id());
            Some(child)
        }
        Err(e) => {
            eprintln!("om_wm: failed to spawn '{cmd}': {e}");
            None
        }
    }
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

// Which corner of a window a point pulls on, as a direction per axis: 1 for the far edge
// (right, bottom), -1 for the near one. split_x and split_y say where each axis divides, as a
// fraction of the window, so 0.5 is down the middle and 0.25 puts the boundary a quarter of
// the way in and hands three quarters of that axis to the far side.
//
// Measured from the window's own edges rather than tested against its bounds, so it still
// answers once the cursor has left the window entirely, which it does for most of a drag.
fn corner_at(
    px: f32,
    py: f32,
    ox: f32,
    oy: f32,
    w: f32,
    h: f32,
    split_x: f32,
    split_y: f32,
) -> (f32, f32) {
    (
        if px < ox + w * split_x { -1.0 } else { 1.0 },
        if py < oy + h * split_y { -1.0 } else { 1.0 },
    )
}

// Where an axis divides for a drag currently holding a given side of it. The side being held
// keeps `hold` of the axis, so the boundary sits out of its way: hold the far side and it
// moves toward the near edge, and the other way round.
//
// Asked with the corner in force rather than the one the drag started on, which is what makes
// it hysteresis instead of a one-time bias: crossing over re-homes to the new corner, whose
// own boundary is then the same distance away on the other side. Between the two the corner
// does not change at all, so with hold at 0.75 the middle half of the window is a band where
// a drag simply keeps doing what it was doing.
fn corner_split(start_edge: f32, hold: f32) -> f32 {
    let hold = hold.clamp(0.5, 0.95);
    if start_edge > 0.0 { 1.0 - hold } else { hold }
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
    // True on the frame Super+Escape was pressed, or three fingers went up.
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
                //
                // And focus alone: the window is not brought to the front. Clicking something to
                // type in it should not rearrange the desk, and on a canvas the stack is mostly a
                // detail of where things overlap rather than a pile you dig through. Raising is
                // still what a Super+drag does, since picking a window up is the moment you did
                // ask for it to be on top.
                let window = wl::state::window_root(state, surf);
                focus_window(state, focused, Some(window));
            }
            None => focus_window(state, focused, None),
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
    // Whether the fingers are moving at all, and where what they are doing is going. Both are
    // decided on the frame a gesture starts and held until it ends.
    //
    // Held, because otherwise the destination is re-decided every frame from things the gesture
    // does not control. Let go of Super halfway through panning the canvas and the rest of the
    // gesture goes to whatever the cursor happens to be over: the canvas stops, and an
    // unfocused window's scroll view takes the tail of your pan. Which is what "the inertia
    // lands in the wrong window" was.
    active: bool,
    to_client: bool,
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
// Forward keys to the focused client, minus the ones Super is holding.
//
// Super is the compositor's modifier: every chord behind it is ours, and a client hearing the
// letter as well means Super+S both takes a screenshot and types an s into whatever has focus.
// Two of them were filtered by name, which only ever covered the two that had been noticed.
// So the rule is the general one: while Super is down nothing you type reaches a client.
//
// Modifiers themselves still go. They are not typing, and xkb's idea of what is held has to
// follow the keyboard or the next client to take focus inherits modifiers nobody is pressing.
//
// sent is what we have told the client is down, so a release only goes where its press went.
// Without it, holding a letter, then pressing Super, then letting the letter go leaves the
// client believing that key is still held: it never hears the release, because by then Super
// was down. That is a key repeating forever in someone's editor.
fn forward_keys(
    state: &mut State,
    kb: &input::Input,
    sent: &mut [bool; input::KEY_CODES],
    time_ms: u32,
) {
    let keyboard = state.keyboard.clone();
    for &(code, pressed) in input::events(kb) {
        let i = code as usize;
        if !input::is_modifier(code) {
            if pressed {
                if input::super_down(kb) {
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

// Where the view has to be for a window to look the way it asks to look, when you send the view
// to it.
//
// 1:1 for an ordinary window that fits, which is its own pixel size and what it was drawn for.
//
// Two things pull away from that.
//
// A maximized window took the shape of the view at the moment you asked for it, so 1:1 would show
// that rectangle at some other scale than the one that made it, which is the single arrangement
// where it is not filling anything. The view fits to it instead and it fills the screen again.
//
// And a window too big for the screen does not fit at 1:1 by definition: sending the view to it
// would put you in the middle of it with its edges off both sides, which is not being shown a
// window, it is being dropped into one. So the view goes out far enough to hold it. Only out,
// never in: a small window is left at its own size rather than blown up to fill a screen it was
// never drawn for.
//
// The fit is the smaller of the two ratios, so the whole window is in view rather than cropped on
// one axis.
fn zoom_for_window(
    windows: &render::Windows,
    surface: &WlSurface,
    set: &settings::Settings,
) -> f32 {
    let Some((w, h)) = render::geo_size(windows, surface) else {
        return set.zoom_default;
    };
    if w <= 0.0 || h <= 0.0 {
        return set.zoom_default;
    }
    let (sw, sh) = (ray::screen_width() as f32, ray::screen_height() as f32);
    let zoom = if render::is_maximized(windows, surface) {
        // Exactly the view, with no margin. Maximized means it was given the shape of the screen,
        // and a maximized window with a gap around it is not maximized, it is a large window.
        (sw / w).min(sh / h)
    } else {
        // A window too big for the screen gets the margin, because here the fit is ours rather
        // than the window's: nothing about the window says it should touch the edges, and the
        // room is what makes it read as a window rather than as a wall. Floored at a pixel, so a
        // silly padding cannot invert the fit.
        let pw = (sw - set.fit_padding_px * 2.0).max(1.0);
        let ph = (sh - set.fit_padding_px * 2.0).max(1.0);
        set.zoom_default.min((pw / w).min(ph / h))
    };
    zoom.clamp(set.zoom_min, set.zoom_max)
}

// Which way a swipe reaches, from the way the fingers went.
//
// Natural is the canvas following your hand, so a swipe down brings down what was above you and the
// view therefore travels up. The other way sends the view where the fingers went. One line, but it
// has to be asked in two places, the slide and the search, and they must never disagree.
fn swipe_way(dir: (f32, f32), set: &settings::Settings) -> (f32, f32) {
    if set.swipe_natural {
        (-dir.0, -dir.1)
    } else {
        dir
    }
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
    let fade_loc = ray::shader_location(shader, "windowAlpha");

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
    // Dumps each toplevel's surface tree, which is what decides how children stack. Twice now
    // a child has drawn on the wrong side of something and the slots were the only way to see
    // what the client had actually asked for.
    let debug_tree = std::env::var("OM_WM_DEBUG_TREE").is_ok();
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
    // When the last middle button press landed, for the double click chord.
    let mut last_middle_ms: u32 = 0;
    // And the last left click on a window, for the Super + double click that brings one to
    // you. The surface comes with it, because two clicks on two different windows are two
    // first clicks rather than a double one.
    // Which keycodes a client has been told are down, so a release always follows its press.
    let mut keys_sent = [false; input::KEY_CODES];
    let mut last_left_ms: u32 = 0;
    let mut last_left_window: Option<WlSurface> = None;
    // Whether Super+S was pressed this frame, and how many shots this run has taken. The
    // number goes in the filename so a second one does not quietly replace the first.
    let mut shot_now = false;
    let mut shots: u32 = 0;
    // A zoom on its way somewhere, or None when nothing is travelling. The anchor is kept
    // rather than re-read so the canvas point it turns around stays put for the whole
    // journey, even if the cursor moves meanwhile.
    //
    // Two things use it. A released pinch heads for 1:1 around the fingers, and the overview
    // heads for overview_zoom around the middle of the screen, which is what "the centre of
    // the camera" means once it is a screen point.
    let mut zoom_ease: Option<Journey> = None;
    // A scale the view was deliberately sent to, when it is not zoom_default: the fit of a window
    // it was told to go to. Remembered so the 1:1 detent leaves it alone.
    //
    // The two disagree otherwise, and it looks like a bug because it is one. Fitting a maximized
    // window with a margin lands near 1:1 by construction, since the window is the size of the
    // screen and the margin is small, and the detent then reads that as a zoom that drifted and
    // pulls it the rest of the way, throwing the margin away in a second journey. A drift is what
    // the detent is for; a fit is not a drift.
    //
    // Only while the zoom is still sitting where it was put. Move it by any means, by a hair, and
    // this stops matching and the detent goes back to work.
    let mut zoom_placed: Option<f32> = None;
    // A three-finger swipe being made rather than having been made: where the view was when the
    // fingers landed, what each direction would do from there, and where the scale is turned
    // around. Held for as long as the fingers are down.
    struct SwipePreview {
        // Where the view was when the fingers landed. Everything the gesture does is measured from
        // here: the slide while it is being made, the direction the search is done in, and the
        // place it comes back to when there is nothing that way.
        from_cx: f32,
        from_cy: f32,
    }
    let mut swipe_preview: Option<SwipePreview> = None;
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
        active: false,
        to_client: false,
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
                    println!("om_wm: cursor image {w}x{h} cached");
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

        // A window that has just drawn its first frame takes the keyboard, so that something
        // you launched is something you can type into without hunting for it.
        //
        // Only from the client that already had focus, or when nothing had it. That is the
        // difference between a dialog opening in front of you, which you want, and an
        // application you left running in the background helping itself to your keystrokes
        // mid-sentence, which is the reason compositors stopped letting clients do this at
        // all. The protocol's own answer for the wider case is xdg_activation_v1, where the
        // app doing the launching hands over a token proving a user asked for it; we do not
        // implement it, so anything that is not the focused client waits to be clicked.
        for surface in windows.mapped.drain(..).collect::<Vec<_>>() {
            let welcome = match set.focus_new {
                settings::FocusNew::Never => false,
                settings::FocusNew::Always => true,
                settings::FocusNew::SameClient => match focused.as_ref() {
                    Some(current) => same_client(current, &surface),
                    None => true,
                },
            };
            if welcome {
                // No raise needed: a window is given the frontmost stack order as it is
                // stored, and nothing between there and here can have changed that.
                focus_window(&mut state, &mut focused, Some(surface.clone()));
                // And the view goes to it, on the same journey a double click makes.
                //
                // A canvas is larger than the screen, so a window that opened in clear space may
                // have opened somewhere you are not looking, and one you launched and cannot find
                // is worse than one that landed on your work. Behind the same welcome test as the
                // focus above, so an application helping itself to a window in the background
                // cannot help itself to your view either.
                if set.spawn_travel {
                    if let Some((wx, wy)) = render::window_center(&windows, &surface) {
                        let target = zoom_for_window(&windows, &surface, &set);
                        zoom_placed = (target != set.zoom_default).then_some(target);
                        zoom_ease = Some(journey_to(&cam, target, Some((wx, wy)), 0.0, 0.0, false));
                    }
                }
            }
        }

        if debug_tree && frame % 120 == 0 {
            for surface in wl::state::toplevel_surfaces(&state) {
                let tree = wl::state::surface_tree(&surface);
                let root_slot = tree.iter().position(|(s, _, _)| *s == surface).unwrap_or(0);
                println!("om_wm: tree {} surfaces, root at slot {root_slot}", tree.len());
                for (slot, (s, ox, oy)) in tree.iter().enumerate() {
                    let size = render::geo_size(&windows, s)
                        .map(|(w, h)| format!("{w:.0}x{h:.0}"))
                        .unwrap_or_else(|| "no entry".to_string());
                    println!(
                        "om_wm:   slot {slot} off {ox:.0},{oy:.0} {size}{}",
                        if slot == root_slot { "  <- root" } else { "" }
                    );
                }
            }
        }

        // And focus a window another client asked us to activate. This is the case our own
        // rule above cannot reach: you clicked a link in one application and a different one
        // opened the page. The token is what makes it safe to honour, and whether to trust it
        // was decided at the protocol boundary; by the time it arrives here it is a window the
        // user asked for.
        //
        // Raised as well as focused, since the point of an activation is that the user wants
        // to see the thing.
        for surface in state.activation_requests.drain(..).collect::<Vec<_>>() {
            if !surface.is_alive() {
                continue;
            }
            render::front(&mut windows, &surface);
            focus_window(&mut state, &mut focused, Some(surface.clone()));
            // And go to it if it is not already all on screen.
            //
            // An application activating one of its windows means "the thing you asked for is here",
            // and here is a place on a canvas larger than the view. Partly visible counts as not
            // here: a window with half of itself off the edge is one you would have to go and find
            // anyway, and the journey is cheaper than the hunt.
            //
            // Fully visible is left alone. The window has focus and you can see all of it, and
            // moving the view then would be motion for its own sake.
            let (sw, sh) = (ray::screen_width() as f32, ray::screen_height() as f32);
            let (lx, ty, rx, by) = camera::view_rect(&cam, sw, sh);
            if let Some((x, y, w, h)) = render::window_rect(&windows, &surface) {
                let all_on_screen = x >= lx && y >= ty && x + w <= rx && y + h <= by;
                if !all_on_screen {
                    if let Some((wx, wy)) = render::window_center(&windows, &surface) {
                        let target = zoom_for_window(&windows, &surface, &set);
                        zoom_placed = (target != set.zoom_default).then_some(target);
                        zoom_ease =
                            Some(journey_to(&cam, target, Some((wx, wy)), 0.0, 0.0, false));
                    }
                }
            }
        }

        // New windows open where the view is.
        render::set_place_origin(
            &mut windows,
            cam.cx,
            cam.cy,
            render::Spawn {
                clear: set.spawn_clear,
                gap: set.spawn_gap,
                order: set.spawn_order,
            },
        );


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

        // Say which way dmabufs are being handled, at startup and on every reload that
        // changes it.
        if announced_dmabuf != Some(set.dmabuf_mode) {
            announced_dmabuf = Some(set.dmabuf_mode);
            match set.dmabuf_mode {
                DmabufMode::Hold => println!("om_wm: dmabuf_mode hold, buffers sampled in place"),
                DmabufMode::Blit => println!(
                    "om_wm: dmabuf_mode blit, buffers copied into textures we own and released at once"
                ),
            }
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
        // Super+Escape. Detected here rather than inside route_input, which used to do it,
        // because half of what it does is the camera's and route_input has no camera.
        let let_go = inp
            .as_ref()
            .map(|i| {
                input::super_down(i)
                    && input::events(i).iter().any(|&(c, p)| p && c == input::KEY_ESC)
            })
            .unwrap_or(false);
        // What the fingers did, not what it did to the camera: where a scroll goes is
        // decided below, alongside the wheel's, so a trackpad can scroll a window.
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
        // And its scroll joins the mouse's, in the same fields libinput fills when it
        // drives the trackpad itself. Everything downstream then treats the two modes
        // identically, including forwarding to whatever the pointer is over.
        //
        // Every window's hold on the keyboard, the pointer and the implicit grab is dropped
        // by the key and by the swipe alike: stepping back to look at everything is not a
        // moment when one window should still be taking your keystrokes.
        // What lets go of every window: Super+Escape, and the three-finger tap.
        //
        // A swipe used to do this, when it meant "step back from everything". It means "go to that
        // one" now and hands over focus rather than dropping it. The tap took the other half of the
        // old meaning: it is a gesture about where the view is, never about what has your typing,
        // so it lets go whichever of its three answers it gives, including the one that flies you
        // into a window. Landing in front of something is not the same as working in it.
        let mut release_all = let_go || pad.three_finger_double_tap;

        // Whether this reaches the canvas or the window under the pointer is decided below,
        // by whether Super is held. The pad reports the same thing either way.
        ptr.scroll_x += pad.scroll_x;
        ptr.scroll_y += pad.scroll_y;
        ptr.pinch *= pad.pinch;
        // Whether the wheel and the fingers belong to the canvas or to the window under the
        // pointer. Super takes them for the canvas; otherwise they go to whatever is hovered.
        // Two fingers belong to a window only while a window is being worked in. Nothing
        // focused means nothing is being worked in, so they go to the canvas instead, and you
        // do not have to reach for Super to move the view you are already looking at.
        //
        // Focus rather than hover, because hover is where the cursor happens to be resting
        // and focus is what you last chose. Click a window and two fingers scroll it; click
        // the canvas and the same two fingers move the canvas.
        let pointer_on_client =
            !super_down && focused.is_some() && hovered.is_some();
        // A finger scroll's destination is that answer taken once, on the frame the fingers
        // start moving, and kept until they lift. The wheel is not latched: a notch is a whole
        // event with nothing to be halfway through.
        let scrolling = ptr.scroll_x != 0.0 || ptr.scroll_y != 0.0;
        let scroll_ended = pad.scroll_ended || ptr.scroll_ended;
        if scrolling && !scroll_gesture.active {
            scroll_gesture.active = true;
            scroll_gesture.to_client = pointer_on_client;
        }
        // The gesture is over when the fingers that made it are no longer both there, or when
        // libinput says they lifted.
        //
        // Not on pad.scroll_ended. That fires as soon as the pan stops leading, which is what a
        // swipe does as it slows down before letting go, and clearing the latch there let the last
        // drift of the same swipe be re-decided against a Super that had already been released.
        // Not on the whole hand leaving either: a two-finger scroll ends when one of the two does,
        // and a palm left on the pad must not keep a finished gesture's destination alive.
        let pad_driving = set.trackpad_mode == input::TrackpadMode::Custom;
        if ptr.scroll_ended || (pad_driving && pad.fingers < 2) {
            scroll_gesture.active = false;
        }
        let scroll_to_client = scroll_gesture.active && scroll_gesture.to_client;
        let scroll_to_canvas = scroll_gesture.active && !scroll_gesture.to_client;
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
        if super_down && ptr.middle {
            cam.cx -= ptr.dx / cam.zoom;
            cam.cy -= ptr.dy / cam.zoom;
        }
        pressed |= ptr.left_pressed;
        released |= ptr.left_released;

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
            // Super+W asks the focused window to close, the way every desktop's close chord does.
            //
            // Asks. xdg_toplevel.close is a request, and a client may answer it with a dialog about
            // unsaved work or ignore it entirely. Killing the process instead would be deciding on
            // its behalf that whatever it was holding does not matter.
            let close_window = input::super_down(i)
                && input::events(i).iter().any(|&(c, p)| p && c == input::KEY_W);
            if close_window {
                if let Some(surf) = focused.clone() {
                    wl::state::close_toplevel(&state, &surf);
                }
            }
            let toggle_pad = input::super_down(i)
                && input::events(i).iter().any(|&(c, p)| p && c == input::KEY_P);
            if toggle_pad {
                debug_pad = !debug_pad;
                println!("om_wm: trackpad overlay {}", if debug_pad { "on" } else { "off" });
            }
            // Super+S grabs the frame. Taken at the end of the draw, so what lands in the
            // file is the frame that was on screen, cursor plane aside: that is a hardware
            // overlay the GPU never composites, so it cannot be in a readback.
            shot_now = input::super_down(i)
                && input::events(i).iter().any(|&(c, p)| p && c == input::KEY_S);
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
        // Super+0 scales around the screen center; the middle click has a cursor
        // to anchor on, so it keeps the canvas under the pointer fixed.
        // A reset says where the zoom should be, so nothing may be on its way somewhere else.
        if reset_click || reset_key {
            zoom_ease = None;
        }
        if reset_click {
            let (cxp, cyp) = pointer_xy.unwrap_or((0, 0));
            camera::reset_zoom_at(
                &mut cam,
                cxp as f32,
                cyp as f32,
                ray::screen_width() as f32,
                ray::screen_height() as f32,
                &set,
            );
        } else if reset_key {
            camera::reset_zoom(&mut cam, &set);
        }

        // Wheel and middle-drag belong either to a client or to the canvas, never
        // to both. Super forces the canvas. This reads the hover from the frame
        // before on purpose: route_input updates it later in the frame, and using
        // one value for both branches is what stops them both firing on the frame
        // the pointer crosses a window edge.
        // Out in the overview the canvas has the pointer already, so two fingers pan and
        // pinch it with nothing held down. In close they belong to the window under the
        // cursor unless Super takes them.
        if super_down || focused.is_none() {
            let (cxp, cyp) = pointer_xy.unwrap_or((0, 0));
            if ptr.wheel != 0.0 {
                // The wheel is a zoom of its own; whatever a pinch was settling toward is
                // not where the wheel is going.
                zoom_ease = None;
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
                // A pinch owns the zoom while it lasts, so anything the last one was still
                // settling toward is no longer where we are going.
                zoom_ease = None;
            }
        }

        // Finger scroll pans the canvas when the gesture belongs to the canvas, whatever the
        // keyboard has done since it started. The camera moves against the fingers so the canvas
        // travels with them, matching touch.rs.
        //
        // Outside the Super test on purpose: that test decided the destination, once, above. A
        // gesture that began on the canvas finishes on the canvas.
        if scroll_to_canvas {
            cam.cx -= ptr.scroll_x / cam.zoom;
            cam.cy += ptr.scroll_y / cam.zoom;
        }

        // Nothing herds the zoom any more. A pinch is left exactly where the fingers left
        // it, at any scale between the limits, which is what makes the canvas free: the view
        // is a place you are rather than one of two places you may be.
        //
        // Which way of working is in force is therefore no longer a question about the zoom.
        // It is said outright, by the three-finger swipes and by Super+Escape below, and it
        // stays said until one of them says otherwise. Zooming inside either one changes what
        // you can see and not what your hands do.

        // Three fingers, and Super+Escape, move the whole canvas rather than a piece of it,
        // so they turn around the middle of the screen rather than the cursor.
        //
        // The fingers name a direction: up steps back far enough to see where everything is,
        // down comes home to 1:1. The key has no direction, so it toggles on where you
        // already are, which makes one chord both "show me everything" and "put me back".
        //
        // Letting go of the windows is the other half of both, and happens in route_input.
        // Not for a swipe down: coming home from the overview leaves focus alone, because by
        // then there is nothing focused to leave.
        // Where a gesture sends the view, if anywhere. Each is a step with somewhere it
        // stops rather than a toggle, so the same gesture repeated walks in one direction and
        // then does nothing more.
        //
        // Two destinations rather than two steppers. Up goes to the overview and down comes home
        // to 1:1, from wherever you are, and each does nothing only when you are already there.
        //
        // Both used to be conditional, and each was missing the case you most want it in. Down
        // answered only from out past 1:1, so zoomed in it did nothing, which is exactly where
        // there is nothing on screen to aim a pinch at. Up was a two-step, in to 1:1 and then out
        // to the overview, so from a scale of your own it took two gestures to see the desk, and
        // from just inside the overview it took two to get out.
        //
        // The reasoning for the old up was that a view you pinched to is a place you chose, and
        // that a gesture should not overrule it. But that is what the gesture is for: it says show
        // me everything, and the answer to it cannot depend on where you happened to be standing.
        // One gesture that always means the same thing beats one that means it half the time.
        //
        // Super+Escape moves nothing. It lets go of every window and leaves the view exactly
        // where you had put it, which is the whole of what it is for: a key with no direction
        // cannot say where to go, and made to guess it moved the canvas out from under work
        // that only wanted the keyboard back.
        // Close enough to 1:1 to count as being there, which is what decides whether a tap steps
        // back or comes in. Wider than the detent's own settling, so a zoom the detent has just
        // finished pulling home is unambiguously home.
        const HOME: f32 = 0.01;

        // The swipe, while it is being made and when it is let go.
        //
        // A swipe means "go that way": to the nearest window in the direction the fingers went, at
        // whatever angle they went in. While they are down the view slides that way, part of the
        // distance, so the gesture is something you can feel doing and can take back by lifting
        // early. Letting go past the threshold commits it.
        //
        // This used to be about the zoom, up to the overview and down to 1:1. Those are the tap's
        // job now: a swipe is for moving between windows, which is the thing you do most and had
        // no gesture at all.
        if pad.swiping {
            let preview = swipe_preview
                .get_or_insert_with(|| SwipePreview { from_cx: cam.cx, from_cy: cam.cy });
            // How far the view goes for how far the fingers went.
            //
            // Up to the trigger it is a straight fraction of the travel, so the gesture feels
            // attached to the hand. Past it the give tails off toward a limit it never reaches:
            // a hand that keeps pulling and gets nothing back stops feeling connected to anything,
            // and a wall is what this exists to avoid. The two pieces meet at the trigger with the
            // same slope, so you cannot feel where the resistance begins, only that it is there.
            let p = pad.swipe_progress;
            let base = set.swipe_preview_frac;
            let travel = if p <= 1.0 {
                base * p
            } else {
                let room = (1.0 + set.swipe_stretch_frac - base).max(0.0001);
                let give = base / set.swipe_resist;
                base + room * (1.0 - 1.0 / (1.0 + (p - 1.0) * give / room))
            };
            // With the canvas or with the view, see swipe_natural. Applied here and to the search
            // below from the same value, so what you feel during the gesture and where it takes you
            // can never disagree.
            let (dx, dy) = swipe_way(pad.swipe_dir, &set);
            // In screen pixels, so the slide looks the same at any zoom, and converted here to the
            // canvas units the camera is in.
            let slide = travel * set.swipe_nudge_px / cam.zoom.max(0.0001);
            cam.cx = preview.from_cx + dx * slide;
            cam.cy = preview.from_cy + dy * slide;
            // The fingers own the view while they are on the pad, so anything it was still
            // travelling toward is no longer where it is going.
            zoom_ease = None;
        }

        // Let go, having gone far enough: the nearest window that way, or back where you started.
        //
        // Searched from where the fingers landed rather than from where the slide has left the
        // view, or the gesture would be biased by its own preview: nudge far enough toward a window
        // and it starts to count as being behind you.
        if pad.swipe_fired {
            let origin = swipe_preview
                .as_ref()
                .map(|pv| (pv.from_cx, pv.from_cy))
                .unwrap_or((cam.cx, cam.cy));
            match render::nearest_in_direction(&windows, origin, swipe_way(pad.swipe_dir, &set)) {
                Some(surf) => {
                    if let Some((wx, wy)) = render::window_center(&windows, &surf) {
                        let window = wl::state::window_root(&state, &surf);
                        focus_window(&mut state, &mut focused, Some(window));
                        let target = zoom_for_window(&windows, &surf, &set);
                        zoom_placed = (target != set.zoom_default).then_some(target);
                        zoom_ease = Some(journey_to(&cam, target, Some((wx, wy)), 0.0, 0.0, true));
                    }
                }
                // Nothing that way. The swipe still happened and still gave way; there was simply
                // nowhere for it to go, so the view comes back rather than drifting off the end.
                None => {
                    zoom_ease = Some(journey_to(&cam, cam.zoom, Some(origin), 0.0, 0.0, true));
                }
            }
            swipe_preview = None;
        }
        // And the fingers left without going far enough. Back to where the view was when they
        // landed, on the same spring as everything else.
        if !pad.swiping {
            if let Some(preview) = swipe_preview.take() {
                let home = (preview.from_cx, preview.from_cy);
                zoom_ease = Some(journey_to(&cam, cam.zoom, Some(home), 0.0, 0.0, true));
            }
        }

        // The detent at 1:1, tested every frame rather than when a pinch ends.
        //
        // A scale like 1.0004 costs every window on screen a resample and shows nothing for
        // it: the pixel grid disengages and text softens for a difference nobody can see. And
        // a pinch ending is not the only way to arrive at one. The wheel, the keyboard, a
        // gesture the canvas only half took, an ease that was interrupted: any of them can
        // leave the zoom beside 1:1 rather than on it. So the question is asked of the zoom
        // itself, every frame, whatever put it there.
        //
        // Not while a pinch is driving: fingers on the pad own the zoom, and the band is
        // theirs to move through. The moment they stop asking for a scale, the zoom takes the
        // rest of the step. And not over an ease already running, which has its own target and
        // its own anchor, and would otherwise be re-anchored to a moving pointer every frame.
        const PLACED: f32 = 0.001;
        let sits_where_placed = zoom_placed.map_or(false, |z| (cam.zoom - z).abs() < PLACED);
        if set.zoom_detent > 0.0
            && zoom_ease.is_none()
            && ptr.pinch == 1.0
            && !pad.swiping
            && !sits_where_placed
        {
            let off = cam.zoom - set.zoom_default;
            if off != 0.0 && off.abs() <= set.zoom_detent {
                let (ax, ay) = pointer_xy.unwrap_or((0, 0));
                zoom_ease = Some(journey_to(&cam, set.zoom_default, None, ax as f32, ay as f32, false));
            }
        }

        if let Some(j) = zoom_ease.as_mut() {
            // Advance the spring. Stiffness and friction from the frequency and damping ratio in
            // the settings, which is the same spring written in the units you can feel: an
            // undamped spring at spring_hz would make that many round trips a second, and the
            // damping says how much of that survives.
            //
            // dt is capped, because a frame that took a long time is a stall rather than a slow
            // moment, and integrating one whole would fling the spring somewhere absurd.
            let dt = ray::frame_time().min(0.05);
            let (hz, damping) = if j.swipe {
                (set.swipe_spring_hz, set.swipe_spring_damping)
            } else {
                (set.spring_hz, set.spring_damping)
            };
            let w = std::f32::consts::TAU * hz;
            let k = w * w;
            let c = 2.0 * damping * w;
            j.v += (k * (1.0 - j.p) - c * j.v) * dt;
            j.p += j.v * dt;

            let (ax, ay, target, to, p) = (j.ax, j.ay, j.target, j.to, j.p);
            // Arrived when the spring is at rest at its destination, not merely passing through
            // it: an underdamped spring crosses the target several times on the way to stopping,
            // and cutting it off at the first crossing is what makes a bounce look like a glitch.
            let landed = (p - 1.0).abs() < 0.001 && j.v.abs() < 0.01;
            // Where the scale is at this progress. Geometric, so halfway is the geometric mean:
            // half of a linear ease from 0.1 to 1.0 is 0.55, which is most of the way there to
            // the eye and a tenth of the way there to the maths.
            let want = if j.from_zoom > 0.0 && target > 0.0 {
                (j.from_zoom * (target / j.from_zoom).powf(p)).clamp(set.zoom_min, set.zoom_max)
            } else {
                target
            };
            let from_off = j.from_off;
            match to {
                // Going somewhere: the centre travels with the scale, at the same rate, so the
                // two arrive together and the window grows into the middle of the view rather
                // than sliding across it afterwards. No screen point is held, because the
                // whole idea is that the view is moving.
                // Going somewhere. The screen offset it started with shrinks by the same
                // progress, and the centre is whatever puts that offset on screen at the scale
                // this progress asks for, so the window closes on the middle of the view at a
                // steady rate rather than sliding into place after the zoom has finished.
                Some((tx, ty)) => {
                    cam.zoom = want;
                    let left = 1.0 - p;
                    cam.cx = tx - from_off.0 * left / cam.zoom;
                    cam.cy = ty - from_off.1 * left / cam.zoom;
                    if landed {
                        cam.zoom = target;
                        cam.cx = tx;
                        cam.cy = ty;
                        zoom_ease = None;
                    }
                }
                // Staying where it is pointed, turning around a screen point: the swipes and the
                // 1:1 detent. Applied as this frame's factor so the anchor is held by the same
                // arithmetic a pinch uses.
                None => {
                    let factor = if cam.zoom > 0.0 { want / cam.zoom } else { 1.0 };
                    camera::zoom_at(
                        &mut cam,
                        factor,
                        ax,
                        ay,
                        ray::screen_width() as f32,
                        ray::screen_height() as f32,
                        &set,
                    );
                    if landed {
                        let factor = if cam.zoom > 0.0 { target / cam.zoom } else { 1.0 };
                        camera::zoom_at(
                            &mut cam,
                            factor,
                            ax,
                            ay,
                            ray::screen_width() as f32,
                            ray::screen_height() as f32,
                            &set,
                        );
                        zoom_ease = None;
                    }
                }
            }
        }

        // Super with plus or minus. Gated on Super inside, like everything else the canvas
        // answers to, so it does not need to care whether a window has the keyboard.
        camera::camera_update(&mut cam, inp.as_ref(), &set);

        render::prune_dead(&mut windows);
        render::animate(&mut windows, ray::frame_time());
        // The view lands on whole pixels whatever moved it: keys, wheel, middle drag,
        // trackpad, or a zoom reset that changed the scale without touching the pan. Applied
        // as the 3D camera is built rather than written back into cam, so no code path can
        // forget it and none can accumulate it either.
        let cam3d = camera::camera_3d(&cam, ray::screen_width(), ray::screen_height(), &set);

        // A focused window that has left the view lets go of the keyboard.
        //
        // Focus is a claim on what you type, and typing into something you cannot see is the way
        // to put a paragraph somewhere you will not find it. On a canvas that is not a rare
        // accident: panning away from a window is an ordinary thing to do, and the window has no
        // idea it happened.
        //
        // Gone means gone entirely, not merely mostly: a window one pixel of which is still on
        // screen is a window you are still working in, and letting go while a corner of it shows
        // would make focus flicker as you pan along an edge.
        //
        // Not while the view is travelling. A journey to a window focuses it as it sets off, when
        // the window is by definition still off screen, so asking this question mid-flight would
        // take the focus back before the view had moved a pixel. What you can see is not a settled
        // fact while the camera is moving, and this rule is about settled facts.
        if let (Some(surf), None) = (focused.clone(), zoom_ease.as_ref()) {
            let (sw, sh) = (ray::screen_width() as f32, ray::screen_height() as f32);
            let (lx, ty, rx, by) = camera::view_rect(&cam, sw, sh);
            if let Some((x, y, w, h)) = render::window_rect(&windows, &surf) {
                if x + w < lx || x > rx || y + h < ty || y > by {
                    focus_window(&mut state, &mut focused, None);
                }
            }
        }
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
        let asked_max: Vec<(WlSurface, bool)> = state.maximize_requests.drain(..).collect();
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
                        "om_wm: resize request {}x{} {} took {} frames (press frame {af}, now {frame}), pointer travelled {:.0},{:.0} since, client says min {}x{} max {}x{}",
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

            // Maximizing, which on an infinite canvas has to be given a meaning rather than
            // inherited from one. There is no screen for a window to fill and no edges to
            // stick to: what there is, at any moment, is the part of the canvas you can see.
            // So maximized means exactly that rectangle, in canvas units, taken once when you
            // ask for it.
            //
            // Fixed in canvas space rather than following the view afterwards, because a
            // window that re-shaped itself every time you panned would be the one thing on
            // the canvas that is not a place. Pan away from a maximized window and you leave
            // it behind, the way you leave everything else behind.
            //
            // The zoom is left alone. A maximized window at 0.6 is a window filling a view
            // you chose to be zoomed out, which is a coherent thing to want, and yanking the
            // scale to 1:1 would be the canvas deciding it knows better.
            for (surf, on) in asked_max {
                // Never at map time. A client that asks to be maximized before it has appeared is
                // asking to open filling the screen, and that is not a state to open in here: the
                // view is a place you are, and a window that seizes it on launch moves you
                // somewhere you did not go. New windows arrive at the size they chose, where the
                // cascade puts them, and can be maximized afterwards by whoever is looking.
                //
                // Refused rather than ignored, because a client that asked is owed a configure and
                // sits waiting for one otherwise. It gets 0x0, which is the protocol's "your size
                // is your own business".
                if on && render::geo_size(&windows, &surf).is_none() {
                    wl::state::decline_maximize(&state, &surf);
                    continue;
                }
                if on {
                    let sw = ray::screen_width() as f32;
                    let sh = ray::screen_height() as f32;
                    // The visible rectangle at the z=0 plane, read through the same
                    // projection the hit tests use, so what fills the view really fills it.
                    let corners = (
                        camera::screen_to_plane(cam3d, 0.0, 0.0, 0.0),
                        camera::screen_to_plane(cam3d, sw, sh, 0.0),
                    );
                    let (Some((lx, ty)), Some((rx, by))) = corners else {
                        continue;
                    };
                    // Read before anything moves: this is what unmaximizing gives back.
                    if let (Some((x, y)), Some((w, h))) = (
                        render::window_origin(&windows, &surf),
                        render::geo_size(&windows, &surf),
                    ) {
                        render::maximize(&mut windows, &surf, lx, ty, x, y, w, h);
                    }
                    // Same housekeeping as a client's own move: an animation still running
                    // would drag the window back off the shape we are about to give it.
                    settling.retain(|(s, ..)| s != &surf);
                    if resize_settle.as_ref().map(|st| &st.surface) == Some(&surf) {
                        resize_settle = None;
                    }
                    render::clear_scale(&mut windows, &surf);
                    render::settle(&mut windows, &surf);
                    render::front(&mut windows, &surf);
                    render::set_window_origin(&mut windows, &surf, lx, ty);
                    wl::state::maximize_toplevel(
                        &state,
                        &surf,
                        (rx - lx).round() as i32,
                        (by - ty).round() as i32,
                        true,
                    );
                } else {
                    // And back to the rectangle it had. A window we never maximized still gets
                    // an answer, at the size it already is, because a client waiting on a
                    // configure it asked for is stuck until it arrives.
                    let restore = render::unmaximize(&mut windows, &surf);
                    if let Some((x, y, _, _)) = restore {
                        render::set_window_origin(&mut windows, &surf, x, y);
                    }
                    let (w, h) = restore
                        .map(|(_, _, w, h)| (w, h))
                        .or_else(|| render::geo_size(&windows, &surf))
                        .unwrap_or((0.0, 0.0));
                    wl::state::maximize_toplevel(
                        &state,
                        &surf,
                        w.round() as i32,
                        h.round() as i32,
                        false,
                    );
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
        let grab_windows = super_down;
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
                        // Which corner the drag holds, from the quadrant the press landed in.
                        // Grab near the top left and the top left is what moves, the way every
                        // other desktop does it, instead of always dragging the bottom right
                        // corner however you grabbed the window.
                        //
                        // The opposite corner is what stays nailed down, and a near edge moving
                        // means the window's origin travels as it grows, which the Resize block
                        // already knows how to do for a client dragging its own edge.
                        //
                        // Always a corner, never a single edge. Splitting into nine rather than
                        // four would add pure horizontal and vertical drags, at the cost of a
                        // middle region where a drag resizes nothing at all.
                        // An even split here: nothing has begun yet, so there is no quadrant
                        // to favour. What it picks becomes the one favoured from now on.
                        let (edge_x, edge_y) = corner_at(gx, gy, ox, oy, w, h, 0.5, 0.5);
                        if debug_input {
                            println!(
                                "om_wm: resize grabbed {:.0},{:.0} into {w:.0}x{h:.0}, holding the {} corner",
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
            // Which corner the drag holds, re-tested every frame against the side of the
            // window the cursor is on. The press decides where it starts and after that it
            // follows your hand: cross the middle and the opposite corner takes over without
            // letting go of the button.
            //
            // The consequence, chosen deliberately: shrinking inward eventually carries the
            // cursor past the middle, the corner changes hands, and the window starts growing
            // the other way instead of shrinking further. Going back the way you came undoes
            // it, so it reads as pushing the window's edges around rather than as a mode.
            //
            // Only for our own drag. A client dragging its own edge named which edge that was,
            // and a request to move the right edge must not turn into moving the top one.
            if alive && r.right_button {
                if let (Some((cx, cy)), Some((ow, oh)), Some((ox, oy))) = (
                    camera::screen_to_plane(cam3d, sxp, syp, 0.0),
                    render::geo_size(&windows, &r.surface),
                    render::window_origin(&windows, &r.surface),
                ) {
                    let (ex, ey) = corner_at(
                        cx,
                        cy,
                        ox,
                        oy,
                        ow,
                        oh,
                        corner_split(r.edge_x, set.resize_corner_hold),
                        corner_split(r.edge_y, set.resize_corner_hold),
                    );
                    if ex != r.edge_x || ey != r.edge_y {
                        if debug_input {
                            println!(
                                "om_wm: resize now holding the {} corner",
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

        // Super + double click on a window brings it to you: the view travels to its centre
        // and to 1:1, so the thing you pointed at is the thing in front of you at the scale it
        // was drawn for. The canvas idiom for "open this", on a desk where opening something
        // means going to it rather than it coming to the front.
        //
        // Ahead of the drag, and it eats the press, so the second click cannot also pick the
        // window up: a double click wobbles by a pixel or two, and a drag that started on it
        // would move the window out from under the view on its way there.
        if pressed && grab_windows && drag.is_none() {
            if let Some((surf, ..)) = render::window_at(&windows, cam3d, sxp, syp) {
                let now_ms = start.elapsed().as_millis() as u32;
                let again = now_ms.saturating_sub(last_left_ms) <= set.double_click_ms
                    && last_left_window.as_ref() == Some(&surf);
                if again {
                    // Consumed, so a third click starts counting again rather than firing.
                    last_left_ms = 0;
                    last_left_window = None;
                    if let Some((wx, wy)) = render::window_center(&windows, &surf) {
                        render::front(&mut windows, &surf);
                        // The press that would have focused this is eaten below, so the focus is
                        // given here instead: being sent to a window is choosing it.
                        let window = wl::state::window_root(&state, &surf);
                        focus_window(&mut state, &mut focused, Some(window));
                        let target = zoom_for_window(&windows, &surf, &set);
                        zoom_placed = (target != set.zoom_default).then_some(target);
                        zoom_ease = Some(journey_to(&cam, target, Some((wx, wy)), 0.0, 0.0, false));
                        pressed = false;
                    }
                } else {
                    last_left_ms = now_ms;
                    last_left_window = Some(surf);
                }
            }
        }

        // Three fingers tapped twice brings the window under the cursor to you, which is the
        // journey Super with a double click makes. One hand, no keyboard, and no click: a tap is
        // fingers landing and leaving without travelling, so it cannot be confused with the
        // three-finger swipes, which are the same fingers going somewhere.
        // The tap is the zoom now that the swipes have stopped being about it, and it is one
        // gesture with three answers rather than three gestures.
        //
        // At 1:1 it steps back to the overview, whatever is under the cursor: from up close the
        // thing you want is almost always to see where everything is, and having to aim at empty
        // canvas to ask for it would be a rule to remember rather than a gesture.
        //
        // From anywhere else it comes in. To the window under the cursor if there is one, at the
        // scale that window asks for, and otherwise to 1:1 where you are pointing, so tapping over
        // empty canvas in the overview drops you into that part of it rather than nowhere.
        if pad.three_finger_double_tap {
            let (px, py) = pointer_xy.unwrap_or((0, 0));
            let (ax, ay) = (px as f32, py as f32);
            let at_home = (cam.zoom - set.zoom_default).abs() < HOME;

            // A window under the cursor that is not the one you are already in. That is the case
            // where a tap means "this one", and it is the only case where the tap hands over focus
            // instead of letting go: you pointed at something and asked for it by name.
            //
            // Whatever the zoom. Pointing at a window says which window far more clearly than the
            // scale says anything, so nothing about how close you are should change the answer.
            let under = render::window_at(&windows, cam3d, sxp, syp).filter(|(surf, ..)| {
                focused.as_ref() != Some(&wl::state::window_root(&state, surf))
            });

            match under {
                Some((surf, ..)) => {
                    if let Some((wx, wy)) = render::window_center(&windows, &surf) {
                        render::front(&mut windows, &surf);
                        let window = wl::state::window_root(&state, &surf);
                        focus_window(&mut state, &mut focused, Some(window));
                        // The tap lets go of everything by default. Not this time: it was asked for
                        // a window, and route_input would take back the focus given here.
                        release_all = false;
                        let target = zoom_for_window(&windows, &surf, &set);
                        zoom_placed = (target != set.zoom_default).then_some(target);
                        zoom_ease =
                            Some(journey_to(&cam, target, Some((wx, wy)), 0.0, 0.0, false));
                    }
                }
                // Nothing new to go to, so the tap is about the view.
                //
                // Working in a window, or pointing at the one you are already in: let go of it and
                // step back a little, since standing at the same scale afterwards would say nothing
                // happened and the overview would say more than you asked.
                //
                // Otherwise it is the zoom's own toggle: at 1:1 step out to the overview, and from
                // anywhere else come home to 1:1 where you are pointing.
                None => {
                    let stepping_back = focused.is_some() && set.tap_step_out < 1.0;
                    let target = if stepping_back {
                        (cam.zoom * set.tap_step_out)
                            .max(set.overview_zoom)
                            .clamp(set.zoom_min, set.zoom_max)
                    } else if at_home {
                        set.overview_zoom
                    } else {
                        set.zoom_default
                    };
                    zoom_placed = None;
                    zoom_ease = Some(journey_to(&cam, target, None, ax, ay, false));
                }
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
                // Before the offsets are taken, since this can change both the window's size
                // and where it is.
                unmaximize_for_drag(&mut windows, &state, &surf, cam3d, gx, gy);
                let (ox, oy) = render::surface_origin(&windows, &surf).unwrap_or((ox, oy));
                // In the overview, picking a window up lifts it toward the camera, which is
                // the canvas idiom for having it in your hand and reads well when you can see
                // the whole desk. At 1:1 it only comes to the front and stays on the plane: a
                // lift there is perspective applied to something already filling the screen,
                // so it swims rather than rises, and it takes the window off the pixel grid
                // while you are close enough to watch that happen.
                //
                // Asked of the mode rather than of the zoom. They agree once a gesture has
                // settled, but mid-pinch the zoom is somewhere in between and a drag started
                // then would decide from a number that is still moving.
                if cam.zoom < set.zoom_default {
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
                            "om_wm: cursor now {w}x{h} hotspot {ox},{oy} (client said {hx},{hy})"
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
                    println!("om_wm: cursor want {want} (was {last_cursor_want})");
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
                                println!("om_wm: no theme cursor for {name}, using our own");
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
        // What is covering the window you are working in, faded so you can see through it. After
        // the drag and the settle, so it judges where things are now rather than where they were.
        render::fade_covers(
            &mut windows,
            focused.as_ref(),
            set.cover_opacity,
            ray::frame_time().min(0.05),
        );
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
        // tells the client in the right order. OM_WM_DEBUG_INPUT=1 still dumps the
        // click and every rect it could have hit.
        if debug_input && pressed {
            println!("om_wm: click screen {sxp:.0},{syp:.0}");
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
                super_down,
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
            if pointer_on_client || scroll_to_client || scroll_ended {
                forward_scroll(
                    &mut state,
                    &ptr,
                    pointer_on_client,
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
                pointer_on_client,
                &mut pinch_active,
                &mut pinch_scale,
                time_ms,
            );
        }
        // Outside the mode gate on purpose: xkb state has to follow the keyboard even when
        // no client is listening, or the next client to get focus inherits modifiers that
        // nobody is holding.
        if let Some(i) = inp.as_ref() {
            forward_keys(&mut state, i, &mut keys_sent, time_ms);
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
        if shot_now {
            shot_now = false;
            shots += 1;
            let path = format!("shot-{shots}.png");
            // Everything queued has to be in the framebuffer before it is read back, or the
            // file is missing whatever the batch was still holding.
            ray::flush_batch();
            ray::take_screenshot(&path);
            println!("om_wm: wrote {path}");
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
