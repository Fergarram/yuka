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
use smithay::input::pointer::{AxisFrame, ButtonEvent, MotionEvent};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
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
    // Canvas point where the drag started, and the window's real size then.
    grab_x: f32,
    grab_y: f32,
    from_w: f32,
    from_h: f32,
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
    ptr: &input::Pointer,
    pressed: bool,
    released: bool,
    time_ms: u32,
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
    if changed || entered {
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
fn forward_keys(state: &mut State, kb: &input::Input, time_ms: u32) {
    // Ask Smithay where the keyboard is pointed rather than trusting our own
    // window focus: a popup grab moves keyboard focus to the menu, and a menu
    // opened by right click on a window nobody clicked first would otherwise get
    // no keys at all, so no arrows and no Escape.
    if state.keyboard.current_focus().is_none() {
        return;
    }
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
    // Active Super+drag: (window, offset from cursor to the window's origin).
    let mut drag: Option<(WlSurface, f32, f32)> = None;
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
        for surface in &committed {
            render::upload_committed(
                &mut windows,
                &mut dmabuf_cache,
                &egl,
                &mut state,
                surface,
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
        for surface in wl::state::toplevel_surfaces(&state) {
            wl::state::send_frame_callbacks(&surface, time_ms);
            for (popup, _, _) in wl::state::popups_of(&surface) {
                wl::state::send_frame_callbacks(&popup, time_ms);
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
        let pointer_on_client = !super_down && hovered.is_some();
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
        if ptr.middle && !pointer_on_client {
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
        if pad.reset_zoom && !super_down {
            camera::reset_zoom(&mut cam, &set);
        }

        // Super+0 scales around the screen center; the middle click has a cursor
        // to anchor on, so it keeps the canvas under the pointer fixed.
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
        if !pointer_on_client {
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

        // WASD and Super +/- drive the camera only when no window has the keyboard,
        // since otherwise they are someone's typing. Nothing to do with the trackpad,
        // whose scroll is routed by where the pointer is.
        if focused.is_none() {
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
        // Menus and subsurfaces are anchored to their parent, so they are placed
        // before anything is hit tested or drawn.
        render::sync_children(&mut windows);
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
        // Super+right-drag resizes. Wayland has nothing to stretch: we send the client
        // a configure with the size we want and it renders at that size in its own time,
        // clamping to whatever it declared as its minimum and maximum. So this drag
        // asks, it does not impose, and a window that stops growing has said no.
        //
        // The window's top-left stays put and the far corner follows the cursor, which
        // is the one mapping that needs no handle to grab.
        if ptr.right_pressed && super_down && resize.is_none() {
            if let Some((surf, ..)) = render::window_at(&windows, cam3d, sxp, syp) {
                if let Some((gx, gy)) = camera::screen_to_plane(cam3d, sxp, syp, 0.0) {
                    let root = wl::state::window_root(&mut state, &surf);
                    if let Some((w, h)) = render::geo_size(&windows, &root) {
                        // A window still settling from a previous resize starts fresh.
                        if resize_settle.as_ref().map(|s| &s.surface) == Some(&root) {
                            resize_settle = None;
                        }
                        render::clear_scale(&mut windows, &root);
                        resize = Some(Resize {
                            surface: root,
                            grab_x: gx,
                            grab_y: gy,
                            from_w: w,
                            from_h: h,
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
                (
                    (r.from_w + (px - r.grab_x)).min(ceil_w).max(floor_w.min(ceil_w)),
                    (r.from_h + (py - r.grab_y)).min(ceil_h).max(floor_h.min(ceil_h)),
                )
            });
            match (ptr.right && alive, want) {
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

        if pressed && super_down && drag.is_none() {
            if let Some((surf, ox, oy)) =
                render::window_at(&windows, cam3d, sxp, syp)
            {
                // Re-grabbing a still-settling window: cancel its settle.
                settling.retain(|(s, ..)| s != &surf);
                // Offset from the window origin to the grabbed point, captured
                // on the z=0 plane (constant in world units, independent of z).
                let (gx, gy) = camera::screen_to_plane(cam3d, sxp, syp, 0.0)
                    .unwrap_or((ox, oy));
                render::raise(&mut windows, &surf);
                drag = Some((surf, ox - gx, oy - gy));
                pressed = false;
            }
        }
        if let Some((surf, offx, offy)) = drag.clone() {
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
                        settling.push((surf.clone(), cx, cy, z));
                    }
                }
                render::settle(&mut windows, &surf);
                drag = None;
                released = false;
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

        // Everything that could move a window has run: the drag, the settle, and the
        // child placement above. Put them all on the pixel grid, then decide how each
        // one is sampled from where it actually ended up.
        render::align_positions(&mut windows, cam.zoom);
        render::prepare_textures(&mut windows, cam.zoom, anisotropy);

        // Dismissal is the popup grab's job now, not ours: it knows the chain and
        // tells the client in the right order. OM_WM_DEBUG_INPUT=1 still dumps the
        // click and every rect it could have hit.
        if debug_input && pressed {
            println!("om_wm: click screen {sxp:.0},{syp:.0}");
            render::log_rects(&windows);
        }

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
            &ptr,
            pressed,
            released,
            start.elapsed().as_millis() as u32,
        );
        let time_ms = start.elapsed().as_millis() as u32;
        if pointer_on_client {
            forward_scroll(&mut state, &ptr, time_ms, &set);
        }
        if let Some(i) = inp.as_ref() {
            forward_keys(&mut state, i, time_ms);
        }

        ray::begin_drawing();
        ray::clear_background(clear);
        render::draw_windows(&windows, cam3d, shader, alpha_loc, swizzle_loc);
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
