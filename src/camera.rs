//
// Camera (Data Oriented zone)
//
// The infinite canvas view. Windows live in canvas coordinates; the camera maps
// canvas -> screen with a pan (canvas point shown at screen center) and a zoom
// (screen pixels per canvas unit). Keyboard: WASD pan, Super +/- zoom. Trackpad
// gestures live in the touch module.
//

use crate::input::{self, Input};
use crate::ray;

//
// Constants
//

const PAN_PX_PER_SEC: f32 = 900.0;
const ZOOM_RATE_PER_SEC: f32 = 2.0;
const ZOOM_MIN: f32 = 0.1;
const ZOOM_MAX: f32 = 8.0;
// Perspective field of view (degrees). Larger = more depth/parallax on lift.
const FOV_DEG: f32 = 40.0;

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
pub fn camera_update(cam: &mut Camera, kb: Option<&Input>) {
    let Some(kb) = kb else {
        return;
    };
    let dt = ray::frame_time();

    let pan = PAN_PX_PER_SEC * dt / cam.zoom;
    if input::down(kb, input::KEY_W) {
        cam.cy -= pan;
    }
    if input::down(kb, input::KEY_S) {
        cam.cy += pan;
    }
    if input::down(kb, input::KEY_A) {
        cam.cx -= pan;
    }
    if input::down(kb, input::KEY_D) {
        cam.cx += pan;
    }

    if input::super_down(kb) {
        let zoom_in =
            input::down(kb, input::KEY_EQUAL) || input::down(kb, input::KEY_KPPLUS);
        let zoom_out =
            input::down(kb, input::KEY_MINUS) || input::down(kb, input::KEY_KPMINUS);
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

// Build the perspective 3D camera. It floats on the -z side of the canvas,
// looking toward +z, at a distance chosen so that at the z=0 plane `zoom` screen
// pixels map to one canvas unit (keeping the 2D zoom controls meaningful).
// Viewing from -z with up = -y makes world x run rightward and y downward on
// screen, matching canvas coordinates directly (no mirror). Windows lift toward
// the viewer along -z.
pub fn camera_3d(cam: &Camera, screen_h: i32) -> ray::Camera3D {
    let half = (FOV_DEG * 0.5).to_radians();
    let dist = screen_h as f32 / (2.0 * cam.zoom * half.tan());
    ray::Camera3D {
        position: ray::Vector3 { x: cam.cx, y: cam.cy, z: -dist },
        target: ray::Vector3 { x: cam.cx, y: cam.cy, z: 0.0 },
        up: ray::Vector3 { x: 0.0, y: -1.0, z: 0.0 },
        fovy: FOV_DEG,
        projection: ray::CAMERA_PERSPECTIVE,
    }
}

// Cast the cursor ray through the perspective camera and intersect a horizontal
// plane at z = plane_z. Returns the canvas (x, y) hit, if any.
pub fn screen_to_plane(
    cam3d: ray::Camera3D,
    sx: f32,
    sy: f32,
    plane_z: f32,
) -> Option<(f32, f32)> {
    let r = ray::screen_to_world_ray(sx, sy, cam3d);
    if r.direction.z.abs() < 1e-6 {
        return None;
    }
    let t = (plane_z - r.position.z) / r.direction.z;
    if t < 0.0 {
        return None;
    }
    Some((r.position.x + t * r.direction.x, r.position.y + t * r.direction.y))
}

