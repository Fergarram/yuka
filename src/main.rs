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
// Invert mouse-wheel direction (Mac-natural). Config toggle for now.
const INVERT_SCROLL: bool = true;
// Canvas pixels panned per horizontal wheel notch, at zoom 1.0.
const HWHEEL_PAN: f32 = 60.0;
// Gap within which a second middle click counts as a double click.
const DOUBLE_CLICK_MS: u32 = 400;
// Pixels a client should scroll per wheel notch, alongside the discrete step.
const WHEEL_STEP_PX: f32 = 15.0;
// How long to sleep per iteration while another VT owns the display.
const VT_IDLE_MS: u64 = 30;

//
// State
//

static RUNNING: AtomicBool = AtomicBool::new(true);

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
    kb: Option<&input::Input>,
    pressed: bool,
    released: bool,
    right_pressed: bool,
    right_released: bool,
    moved: bool,
    time_ms: u32,
) {
    let Some((cxp, cyp)) = cursor_pos else { return };
    let (ccx, ccy) = camera::screen_to_plane(cam3d, cxp as f32, cyp as f32, 0.0)
        .unwrap_or((0.0, 0.0));
    let loc = Point::<f64, Logical>::from((ccx as f64, ccy as f64));
    let pointer = state.pointer.clone();
    let keyboard = state.keyboard.clone();

    let leave = |state: &mut State, focused: &mut Option<WlSurface>| {
        let serial = SERIAL_COUNTER.next_serial();
        pointer.motion(state, None, &MotionEvent { location: loc, serial, time: time_ms });
        pointer.frame(state);
        keyboard.set_focus(state, None, SERIAL_COUNTER.next_serial());
        *focused = None;
    };

    // Super+Escape unfocuses.
    let super_escape = kb
        .map(|kb| input::super_down(kb) && input::down(kb, input::KEY_ESC))
        .unwrap_or(false);
    if super_escape && focused.is_some() {
        leave(state, focused);
    }

    // Click: focus the window under the cursor (and forward the press), or
    // unfocus when clicking empty canvas.
    if pressed {
        match render::window_at(windows, cam3d, cxp as f32, cyp as f32) {
            Some((surf, ox, oy)) => {
                render::front(windows, &surf);
                *focused = Some(surf.clone());
                keyboard.set_focus(state, Some(surf.clone()), SERIAL_COUNTER.next_serial());
                let origin = Point::<f64, Logical>::from((ox as f64, oy as f64));
                let serial = SERIAL_COUNTER.next_serial();
                pointer.motion(state, Some((surf.clone(), origin)), &MotionEvent { location: loc, serial, time: time_ms });
                let serial = SERIAL_COUNTER.next_serial();
                pointer.button(state, &ButtonEvent { serial, time: time_ms, button: BTN_LEFT, state: ButtonState::Pressed });
                pointer.frame(state);
            }
            None => {
                if focused.is_some() {
                    leave(state, focused);
                }
            }
        }
    }

    // While focused, forward pointer motion (hover/drag) into that window, but
    // only when the cursor actually moved. Forwarding every frame makes clients
    // treat a still pointer as continuous motion (e.g. weston-smoke never stops
    // emitting).
    if let Some(surf) = focused.clone() {
        match render::window_origin(windows, &surf) {
            Some((ox, oy)) if moved => {
                let origin = Point::<f64, Logical>::from((ox as f64, oy as f64));
                let serial = SERIAL_COUNTER.next_serial();
                pointer.motion(state, Some((surf.clone(), origin)), &MotionEvent { location: loc, serial, time: time_ms });
                pointer.frame(state);
            }
            Some(_) => {} // focused, cursor still: nothing to forward
            None => leave(state, focused), // focused window went away
        }
    }

    if released && focused.is_some() {
        let serial = SERIAL_COUNTER.next_serial();
        pointer.button(state, &ButtonEvent { serial, time: time_ms, button: BTN_LEFT, state: ButtonState::Released });
        pointer.frame(state);
    }

    // Right button goes to whatever is already focused (context menus), it never
    // changes focus itself.
    if focused.is_some() && (right_pressed || right_released) {
        if right_pressed {
            let serial = SERIAL_COUNTER.next_serial();
            pointer.button(state, &ButtonEvent { serial, time: time_ms, button: BTN_RIGHT, state: ButtonState::Pressed });
        }
        if right_released {
            let serial = SERIAL_COUNTER.next_serial();
            pointer.button(state, &ButtonEvent { serial, time: time_ms, button: BTN_RIGHT, state: ButtonState::Released });
        }
        pointer.frame(state);
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

// Forward scrolling to the focused window: inside a window the wheel belongs to
// the client, not to the canvas zoom. Wheels send discrete v120 steps plus the
// pixel value clients expect, finger scroll sends pixels. Our sign convention is
// positive up and right, Wayland's is positive down and right, so the vertical
// axes flip back here.
fn forward_scroll(state: &mut State, ptr: &input::Pointer, time_ms: u32) {
    let pointer = state.pointer.clone();
    if ptr.wheel != 0.0 || ptr.hwheel != 0.0 {
        let mut frame = AxisFrame::new(time_ms).source(AxisSource::Wheel);
        if ptr.wheel != 0.0 {
            let v = -ptr.wheel;
            frame = frame
                .v120(Axis::Vertical, (v * 120.0) as i32)
                .value(Axis::Vertical, (v * WHEEL_STEP_PX) as f64);
        }
        if ptr.hwheel != 0.0 {
            frame = frame
                .v120(Axis::Horizontal, (ptr.hwheel * 120.0) as i32)
                .value(Axis::Horizontal, (ptr.hwheel * WHEEL_STEP_PX) as f64);
        }
        pointer.axis(state, frame);
        pointer.frame(state);
    }
    if ptr.scroll_x != 0.0 || ptr.scroll_y != 0.0 {
        let mut frame = AxisFrame::new(time_ms).source(AxisSource::Finger);
        if ptr.scroll_y != 0.0 {
            frame = frame.value(Axis::Vertical, -ptr.scroll_y as f64);
        }
        if ptr.scroll_x != 0.0 {
            frame = frame.value(Axis::Horizontal, ptr.scroll_x as f64);
        }
        pointer.axis(state, frame);
        pointer.frame(state);
    }
}

// Forward keyboard press/release edges to the focused window. Super+Escape is a
// compositor shortcut (unfocus, handled in route_input) so it is not forwarded.
fn forward_keys(
    state: &mut State,
    kb: &input::Input,
    focused: &Option<WlSurface>,
    time_ms: u32,
) {
    if focused.is_none() {
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

    ray::init_window(0, 0, "om_wm");
    // No SetTargetFPS: the DRM page flip in EndDrawing already vsyncs to the
    // display. A second 60 Hz cap would beat against it and cause stutter.

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

    let egl = egl::init().expect("egl init");
    let dmabuf_formats = egl.query_formats();
    let render_node_dev = egl.render_node_dev();
    println!("om_wm: egl reports {} dmabuf format/modifier pairs", dmabuf_formats.len());

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
    let mut cam = camera::camera_new();
    let mut cursor = cursor::init(ray::screen_width(), ray::screen_height());

    // Session control. With it, logind owns our VT (K_OFF, KD_GRAPHICS) so the
    // switch chords are ours to implement and input needs no grab.
    //
    // Taking control makes us the only thing that can switch VTs, so we refuse it
    // when the machine has no keyboard to switch with: silencing the console then
    // would leave no way off this VT at all. OM_WM_NO_SEAT opts out by hand, which
    // is also what non-interactive runs want.
    let no_seat = std::env::var("OM_WM_NO_SEAT").is_ok();
    let mut seat = if input::any_keyboard_present() && !no_seat {
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

    // libinput drives keyboards and pointers. Without a session it grabs them as
    // it opens them, since the console would otherwise read everything we type,
    // and that costs us VT switching: ctrl+alt+backspace is then the way out.
    let mut inp = input::init(seat.is_none());
    if seat.is_none() && inp.is_some() {
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
    // Last cursor position we forwarded, to avoid spamming clients with motion
    // events every frame while the pointer is still (weston-smoke, for one,
    // emits smoke on every motion event it receives).
    let mut last_cursor: Option<(i32, i32)> = None;
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
    let mut slow_frames: u32 = 0;
    // Last session state we acted on, to catch activation edges from libseat.
    let mut session_active = true;
    // When the last middle button press landed, for the double click chord.
    let mut last_middle_ms: u32 = 0;

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
                        touch::reset(tp);
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

        // Menus follow the window they belong to, every frame.
        render::sync_popups(&mut windows);

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
            touchpad = inp.as_ref().and_then(input::trackpad_node).and_then(touch::open);
        }

        let super_down = inp.as_ref().map(input::super_down).unwrap_or(false);
        let gestures_enabled = focused.is_none();
        // Suppress trackpad gestures while Super is held (reserved for window
        // manipulation) so a pinch does not zoom the canvas.
        let (mut pressed, mut released) = match touchpad.as_mut() {
            Some(tp) => touch::update(
                tp,
                &mut cam,
                cursor.as_mut(),
                gestures_enabled && !super_down,
            ),
            None => (false, false),
        };

        // Pointers, as libinput reports them: motion moves the cursor, clicks add
        // to the click edges, the wheel zooms at the cursor when unfocused. In
        // Libinput trackpad mode the scroll and pinch fields carry the trackpad
        // too; in Custom mode they stay zero because the device is muted there.
        let ptr = inp.as_ref().map(input::pointer).unwrap_or_default();
        if let Some(cur) = cursor.as_mut() {
            cursor::move_by(cur, ptr.dx as i32, ptr.dy as i32);
        }
        // Wheel-click drag also pans; moving the cursor by the same delta keeps
        // the grabbed canvas point exactly under the cursor.
        if ptr.middle && gestures_enabled {
            cam.cx -= ptr.dx / cam.zoom;
            cam.cy -= ptr.dy / cam.zoom;
        }
        pressed |= ptr.left_pressed;
        released |= ptr.left_released;

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
            if super_down && now_ms.saturating_sub(last_middle_ms) <= DOUBLE_CLICK_MS {
                reset_click = true;
                // Consumed, so a third click does not reset again.
                last_middle_ms = 0;
            } else {
                last_middle_ms = now_ms;
            }
        }
        // Super+0 scales around the screen center; the middle click has a cursor
        // to anchor on, so it keeps the canvas under the pointer fixed.
        if reset_click {
            let (cxp, cyp) = cursor.as_ref().map(cursor::pos).unwrap_or((0, 0));
            camera::reset_zoom_at(
                &mut cam,
                cxp as f32,
                cyp as f32,
                ray::screen_width() as f32,
                ray::screen_height() as f32,
            );
        } else if reset_key {
            camera::reset_zoom(&mut cam);
        }

        if gestures_enabled {
            let (cxp, cyp) = cursor.as_ref().map(cursor::pos).unwrap_or((0, 0));
            if ptr.wheel != 0.0 {
                let wheel = if INVERT_SCROLL { -ptr.wheel } else { ptr.wheel };
                camera::zoom_at(
                    &mut cam,
                    1.15_f32.powf(wheel),
                    cxp as f32,
                    cyp as f32,
                    ray::screen_width() as f32,
                    ray::screen_height() as f32,
                );
            }
            if ptr.hwheel != 0.0 {
                let hwheel = if INVERT_SCROLL { -ptr.hwheel } else { ptr.hwheel };
                cam.cx += hwheel * HWHEEL_PAN / cam.zoom;
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
                );
            }
        }

        if gestures_enabled {
            camera::camera_update(&mut cam, inp.as_ref());
        }
        render::prune_dead(&mut windows);
        render::animate(&mut windows, ray::frame_time());
        let cam3d = camera::camera_3d(&cam, ray::screen_height());
        let cursor_pos = cursor.as_ref().map(cursor::pos);

        // Super+drag: grab the window under the cursor and lift it toward the
        // camera. Consumes the click so it is not also focused/forwarded. The
        // window is positioned by projecting the cursor onto the plane at its
        // current lifted z, so the grabbed point stays under the cursor at any
        // zoom (projecting onto z=0 while drawing lifted makes it out-run the
        // cursor via perspective parallax).
        let (sxp, syp) = cursor_pos
            .map(|(x, y)| (x as f32, y as f32))
            .unwrap_or((0.0, 0.0));
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

        // A click that misses every popup closes the open menus. Proper menus take
        // a popup grab and get this from the grab; we do not hold one yet, so the
        // dismissal is ours to do.
        if pressed && wl::state::any_popup(&state) {
            let on_popup = render::window_at(&windows, cam3d, sxp, syp)
                .map(|(surf, _, _)| wl::state::is_popup(&state, &surf))
                .unwrap_or(false);
            if !on_popup {
                wl::state::dismiss_popups(&state);
            }
        }

        let moved = cursor_pos != last_cursor;
        last_cursor = cursor_pos;
        route_input(
            &mut state,
            &mut windows,
            cam3d,
            cursor_pos,
            &mut focused,
            inp.as_ref(),
            pressed,
            released,
            ptr.right_pressed,
            ptr.right_released,
            moved,
            start.elapsed().as_millis() as u32,
        );
        if focused.is_some() {
            forward_scroll(&mut state, &ptr, start.elapsed().as_millis() as u32);
        }
        if let Some(i) = inp.as_ref() {
            forward_keys(
                &mut state,
                i,
                &focused,
                start.elapsed().as_millis() as u32,
            );
        }

        ray::begin_drawing();
        ray::clear_background(clear);
        render::draw_windows(&windows, cam3d, shader, alpha_loc, swizzle_loc);
        if screenshot && frame == shot_frame {
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
