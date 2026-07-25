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
mod kbd;
mod mouse;
mod ray;
mod render;
mod touch;
mod wl;

use std::ffi::c_int;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use smithay::backend::input::{ButtonState, KeyState};
use smithay::input::keyboard::{FilterResult, Keycode};
use smithay::input::pointer::{ButtonEvent, MotionEvent};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, SERIAL_COUNTER};

use camera::Camera;
use render::Windows;
use wl::state::State;

//
// Constants
//

const SHOT_FRAME: u32 = 200;
const BTN_LEFT: u32 = 0x110;
// Invert mouse-wheel direction (Mac-natural). Config toggle for now.
const INVERT_SCROLL: bool = true;

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
    windows: &Windows,
    cam: &Camera,
    cursor_pos: Option<(i32, i32)>,
    focused: &mut Option<WlSurface>,
    kb: Option<&kbd::Keyboard>,
    pressed: bool,
    released: bool,
    time_ms: u32,
) {
    let Some((cxp, cyp)) = cursor_pos else { return };
    let sw = ray::screen_width();
    let sh = ray::screen_height();
    let (ccx, ccy) = camera::screen_to_canvas(cam, cxp as f32, cyp as f32, sw, sh);
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
        .map(|kb| kbd::super_down(kb) && kbd::down(kb, kbd::KEY_ESC))
        .unwrap_or(false);
    if super_escape && focused.is_some() {
        leave(state, focused);
    }

    // Click: focus the window under the cursor (and forward the press), or
    // unfocus when clicking empty canvas.
    if pressed {
        match render::window_at(windows, ccx, ccy) {
            Some((surf, ox, oy)) => {
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

    // While focused, forward pointer motion (hover/drag) into that window.
    if let Some(surf) = focused.clone() {
        match render::window_origin(windows, &surf) {
            Some((ox, oy)) => {
                let origin = Point::<f64, Logical>::from((ox as f64, oy as f64));
                let serial = SERIAL_COUNTER.next_serial();
                pointer.motion(state, Some((surf.clone(), origin)), &MotionEvent { location: loc, serial, time: time_ms });
                pointer.frame(state);
            }
            None => leave(state, focused), // focused window went away
        }
    }

    if released && focused.is_some() {
        let serial = SERIAL_COUNTER.next_serial();
        pointer.button(state, &ButtonEvent { serial, time: time_ms, button: BTN_LEFT, state: ButtonState::Released });
        pointer.frame(state);
    }
}

// Forward keyboard press/release edges to the focused window. Super+Escape is a
// compositor shortcut (unfocus, handled in route_input) so it is not forwarded.
fn forward_keys(
    state: &mut State,
    kb: &kbd::Keyboard,
    focused: &Option<WlSurface>,
    time_ms: u32,
) {
    if focused.is_none() {
        return;
    }
    let keyboard = state.keyboard.clone();
    for &(code, pressed) in kbd::events(kb) {
        if code == kbd::KEY_ESC && kbd::super_down(kb) {
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
        wl::state::init(dmabuf_formats, render_node_dev).expect("wayland init");
    println!("om_wm: WAYLAND_DISPLAY={}", server.socket_name);

    let mut windows = render::windows_new();
    let mut dmabuf_cache = render::dmabuf_cache_new();
    let mut cam = camera::camera_new();
    let mut touchpad = touch::open();
    let mut mouse = mouse::open();
    let mut cursor = cursor::init(ray::screen_width(), ray::screen_height());
    let mut keyboard = kbd::open();
    // The window we are interacting with; while Some, pan/zoom is disabled.
    let mut focused: Option<WlSurface> = None;

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
    let screenshot = std::env::var("OM_WM_SHOT").is_ok();
    let max_frames: u32 = std::env::var("OM_WM_MAX_FRAMES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(u32::MAX);
    let mut last = Instant::now();
    let mut max_dt_ms: f64 = 0.0;
    let mut slow_frames: u32 = 0;

    while RUNNING.load(Ordering::Relaxed)
        && !ray::window_should_close()
        && frame < max_frames
    {
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

        // Send frame callbacks and flush BEFORE the vsync-blocking draw, so the
        // client renders its next frame concurrently with our page flip instead
        // of waiting a full refresh (which would halve its frame rate).
        let time_ms = start.elapsed().as_millis() as u32;
        for surface in wl::state::toplevel_surfaces(&state) {
            wl::state::send_frame_callbacks(&surface, time_ms);
        }
        wl::state::flush(&mut server);

        if let Some(kb) = keyboard.as_mut() {
            kbd::poll(kb);
        }
        let gestures_enabled = focused.is_none();
        let (mut pressed, mut released) = match touchpad.as_mut() {
            Some(tp) => {
                touch::update(tp, &mut cam, cursor.as_mut(), gestures_enabled)
            }
            None => (false, false),
        };

        // External mouse: relative motion moves the cursor, click adds to the
        // click edges, wheel zooms the canvas at the cursor when unfocused.
        if let Some(m) = mouse.as_mut() {
            let mf = mouse::poll(m);
            if let Some(cur) = cursor.as_mut() {
                cursor::move_by(cur, mf.dx, mf.dy);
            }
            // Wheel-click drag also pans; moving the cursor by the same delta
            // keeps the grabbed canvas point exactly under the cursor.
            if mf.middle && gestures_enabled {
                cam.cx -= mf.dx as f32 / cam.zoom;
                cam.cy -= mf.dy as f32 / cam.zoom;
            }
            pressed |= mf.pressed;
            released |= mf.released;
            if gestures_enabled && mf.wheel != 0 {
                let (cxp, cyp) = cursor.as_ref().map(cursor::pos).unwrap_or((0, 0));
                let wheel = if INVERT_SCROLL { -mf.wheel } else { mf.wheel };
                let factor = 1.15_f32.powi(wheel);
                camera::zoom_at(
                    &mut cam,
                    factor,
                    cxp as f32,
                    cyp as f32,
                    ray::screen_width() as f32,
                    ray::screen_height() as f32,
                );
            }
        }

        if gestures_enabled {
            camera::camera_update(&mut cam, keyboard.as_ref());
        }
        render::prune_dead(&mut windows);
        let cursor_pos = cursor.as_ref().map(cursor::pos);
        route_input(
            &mut state,
            &windows,
            &cam,
            cursor_pos,
            &mut focused,
            keyboard.as_ref(),
            pressed,
            released,
            start.elapsed().as_millis() as u32,
        );
        if let Some(kb) = keyboard.as_ref() {
            forward_keys(
                &mut state,
                kb,
                &focused,
                start.elapsed().as_millis() as u32,
            );
        }

        ray::begin_drawing();
        ray::clear_background(clear);
        render::draw_toplevels(&windows, &state, &cam, shader, alpha_loc, swizzle_loc);
        if screenshot && frame == SHOT_FRAME {
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
}
