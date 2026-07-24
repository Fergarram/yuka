//
// om_wm
//
// Wayland compositor + window manager on an infinite canvas, rendered with
// raylib on DRM/KMS. Current milestone: import client buffers (shm copy and
// zero-copy dmabuf) into GL textures and draw them as shader-processed quads.
// Infinite canvas and input come next.
//

mod egl;
mod ray;
mod render;
mod wl;

use std::ffi::c_int;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

//
// Constants
//

const SHOT_FRAME: u32 = 200;

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
    let client_cmd =
        std::env::args().nth(1).unwrap_or_else(|| "weston-simple-egl".to_string());
    let mut child = spawn_client(&server.socket_name, &client_cmd);

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

        ray::begin_drawing();
        ray::clear_background(clear);
        render::draw_toplevels(&windows, &state, shader, alpha_loc, swizzle_loc);
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
    if let Some(mut c) = child.take() {
        let _ = c.kill();
        let _ = c.wait();
    }
    render::destroy_owned(&mut windows);
    dmabuf_cache.destroy_all(&egl);
    ray::unload_shader(shader);
    ray::close_window();
}
