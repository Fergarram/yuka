//
// Camera (Data Oriented zone)
//
// The infinite canvas view. Windows live in canvas coordinates; the camera maps
// canvas -> screen with a pan (canvas point shown at screen center) and a zoom
// (screen pixels per canvas unit). Keyboard: WASD pan, Super +/- zoom. Trackpad
// gestures live in the touch module.
//

use crate::kbd::{self, Keyboard};
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
pub fn camera_update(cam: &mut Camera, kb: Option<&Keyboard>) {
    let Some(kb) = kb else {
        return;
    };
    let dt = ray::frame_time();

    let pan = PAN_PX_PER_SEC * dt / cam.zoom;
    if kbd::down(kb, kbd::KEY_W) {
        cam.cy -= pan;
    }
    if kbd::down(kb, kbd::KEY_S) {
        cam.cy += pan;
    }
    if kbd::down(kb, kbd::KEY_A) {
        cam.cx -= pan;
    }
    if kbd::down(kb, kbd::KEY_D) {
        cam.cx += pan;
    }

    if kbd::super_down(kb) {
        let zoom_in =
            kbd::down(kb, kbd::KEY_EQUAL) || kbd::down(kb, kbd::KEY_KPPLUS);
        let zoom_out =
            kbd::down(kb, kbd::KEY_MINUS) || kbd::down(kb, kbd::KEY_KPMINUS);
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

// Zoom by `factor` while keeping the canvas point currently under the screen
// pixel (sx, sy) fixed there. Used for cursor-anchored pinch zoom.
pub fn zoom_at(cam: &mut Camera, factor: f32, sx: f32, sy: f32, sw: f32, sh: f32) {
    // Canvas point under (sx, sy) at the current zoom.
    let px = cam.cx + (sx - sw * 0.5) / cam.zoom;
    let py = cam.cy + (sy - sh * 0.5) / cam.zoom;
    cam.zoom = (cam.zoom * factor).clamp(ZOOM_MIN, ZOOM_MAX);
    // Re-center so that point stays under (sx, sy).
    cam.cx = px - (sx - sw * 0.5) / cam.zoom;
    cam.cy = py - (sy - sh * 0.5) / cam.zoom;
}

// Map a screen pixel position back to a canvas point (inverse of the camera).
pub fn screen_to_canvas(
    cam: &Camera,
    sx: f32,
    sy: f32,
    screen_w: i32,
    screen_h: i32,
) -> (f32, f32) {
    let cx = cam.cx + (sx - screen_w as f32 * 0.5) / cam.zoom;
    let cy = cam.cy + (sy - screen_h as f32 * 0.5) / cam.zoom;
    (cx, cy)
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
