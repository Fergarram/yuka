//
// Camera (Data Oriented zone)
//
// The infinite canvas view. Windows live in canvas coordinates; the camera maps
// canvas -> screen with a pan (canvas point shown at screen center) and a zoom
// (screen pixels per canvas unit). Driven by keyboard for now (WASD pan, Ctrl +
// / Ctrl - zoom); trackpad/mouse gestures come later.
//

use crate::ray;

//
// Constants
//

const PAN_PX_PER_SEC: f32 = 900.0;
const ZOOM_RATE_PER_SEC: f32 = 2.0;
const ZOOM_MIN: f32 = 0.1;
const ZOOM_MAX: f32 = 8.0;

//
// Types
//

pub struct Camera {
    // Canvas coordinate displayed at the center of the screen.
    pub cx: f32,
    pub cy: f32,
    // Screen pixels per canvas unit.
    pub zoom: f32,
}

//
// Functions
//

pub fn camera_new() -> Camera {
    // Start at native scale, framed on the first window's row.
    Camera { cx: 400.0, cy: 220.0, zoom: 1.0 }
}

// Advance the camera from held keys. Pan speed is constant in screen pixels, so
// it feels the same at any zoom; zoom is exponential and keeps the screen center
// fixed.
pub fn camera_update(cam: &mut Camera) {
    let dt = ray::frame_time();
    if dt <= 0.0 {
        return;
    }

    let pan = PAN_PX_PER_SEC * dt / cam.zoom;
    if ray::is_key_down(ray::KEY_W) {
        cam.cy -= pan;
    }
    if ray::is_key_down(ray::KEY_S) {
        cam.cy += pan;
    }
    if ray::is_key_down(ray::KEY_A) {
        cam.cx -= pan;
    }
    if ray::is_key_down(ray::KEY_D) {
        cam.cx += pan;
    }

    let ctrl = ray::is_key_down(ray::KEY_LEFT_CONTROL)
        || ray::is_key_down(ray::KEY_RIGHT_CONTROL);
    if ctrl {
        let zoom_in = ray::is_key_down(ray::KEY_EQUAL)
            || ray::is_key_down(ray::KEY_KP_ADD);
        let zoom_out = ray::is_key_down(ray::KEY_MINUS)
            || ray::is_key_down(ray::KEY_KP_SUBTRACT);
        let step = 1.0 + ZOOM_RATE_PER_SEC * dt;
        if zoom_in {
            cam.zoom *= step;
        }
        if zoom_out {
            cam.zoom /= step;
        }
        cam.zoom = cam.zoom.clamp(ZOOM_MIN, ZOOM_MAX);
    }
}

// Map a canvas point to a screen pixel position.
pub fn canvas_to_screen(
    cam: &Camera,
    screen_w: i32,
    screen_h: i32,
    x: f32,
    y: f32,
) -> (f32, f32) {
    let sx = (x - cam.cx) * cam.zoom + screen_w as f32 * 0.5;
    let sy = (y - cam.cy) * cam.zoom + screen_h as f32 * 0.5;
    (sx, sy)
}
