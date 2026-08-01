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
use crate::settings::Settings;

//
// Constants
//

// Perspective field of view (degrees). Larger = more depth/parallax on lift.

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

pub fn camera_new(set: &Settings) -> Camera {
    // Start at native scale, framed on the first window's row.
    Camera { cx: 400.0, cy: 220.0, zoom: set.zoom_default }
}

// Back to 1:1. The canvas point at the screen center stays put, so the view
// scales around whatever you were looking at.
pub fn reset_zoom(cam: &mut Camera, set: &Settings) {
    cam.zoom = set.zoom_default;
}

// Back to 1:1 around a screen point, so the canvas under that point stays put.
pub fn reset_zoom_at(cam: &mut Camera, sx: f32, sy: f32, sw: f32, sh: f32, set: &Settings) {
    zoom_at(cam, set.zoom_default / cam.zoom, sx, sy, sw, sh, set);
}

// Advance the camera from held keys. Pan speed is constant in screen pixels, so
// it feels the same at any zoom; zoom is exponential and keeps the screen center
// fixed.
pub fn camera_update(cam: &mut Camera, kb: Option<&Input>, set: &Settings) {
    let Some(kb) = kb else {
        return;
    };
    let dt = ray::frame_time();

    // Not while Super is held: that is the compositor's modifier, and the chords hanging off
    // it (Super+S for a screenshot, among others) would otherwise pan the view as a side
    // effect of being pressed.
    let pan = if input::super_down(kb) { 0.0 } else { set.pan_px_per_sec * dt / cam.zoom };
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
        let step = 1.0 + set.zoom_rate_per_sec * dt;
        if zoom_in {
            cam.zoom *= step;
        }
        if zoom_out {
            cam.zoom /= step;
        }
        cam.zoom = cam.zoom.clamp(set.zoom_min, set.zoom_max);
    }
}

// Zoom by `factor` while keeping the canvas point currently under the screen
// pixel (sx, sy) fixed there. Used for cursor-anchored pinch zoom.
pub fn zoom_at(cam: &mut Camera, factor: f32, sx: f32, sy: f32, sw: f32, sh: f32, set: &Settings) {
    // Canvas point under (sx, sy) at the current zoom.
    let px = cam.cx + (sx - sw * 0.5) / cam.zoom;
    let py = cam.cy + (sy - sh * 0.5) / cam.zoom;
    cam.zoom = (cam.zoom * factor).clamp(set.zoom_min, set.zoom_max);
    // Re-center so that point stays under (sx, sy).
    cam.cx = px - (sx - sw * 0.5) / cam.zoom;
    cam.cy = py - (sy - sh * 0.5) / cam.zoom;
}

// Whether pixel alignment is worth doing at this zoom, which is the one question both the
// view and the windows have to answer the same way.
//
// At 1:1 and below. There a canvas unit is at most one screen pixel, so the grid is what
// keeps the canvas on whole pixels and a window sampled texel for texel rather than smeared
// across two columns.
//
// Above 1:1 it is deliberately let go. Magnified, half a pixel of offset is a fraction of a
// source texel, far below anything the content itself resolves to, and holding the view to a
// grid that has become coarser than the pixels it is drawn with buys nothing.
const NATIVE_ZOOM_EPS: f32 = 0.001;

pub fn snaps_to_pixels(zoom: f32) -> bool {
    zoom <= 1.0 + NATIVE_ZOOM_EPS
}

// The view's centre, landed on whole pixels. Derived every frame and never written back, so
// cx and cy stay exactly where panning and zooming put them.
//
// At the z=0 plane a canvas point maps to screen as x * zoom - cx * zoom + sw/2, so what has
// to be an integer is (cx * zoom - sw/2): then every canvas coordinate that is itself an
// integer lands on a pixel boundary, and a window at rest is sampled 1:1 instead of being
// resampled across two columns. Carrying the screen half-size through is what makes that
// right on an odd sized screen, where the centre itself sits on a half pixel.
//
// Only the pan. A fractional zoom cannot put every canvas unit on a pixel, and rounding the
// zoom itself would fight the gesture that set it.
//
// This used to assign to cam.cx, which was a ratchet: the grid is 1/zoom wide, a pinch changes
// zoom every frame, and each frame re-rounded the previous frame's rounded value against a
// grid that had moved. One pinch out to 0.3 and back walked the view 22 canvas units, and the
// windows were doing the same thing to themselves alongside it (see Windows::draw_x).
pub fn snapped_center(cam: &Camera, sw: f32, sh: f32) -> (f32, f32) {
    if !snaps_to_pixels(cam.zoom) {
        return (cam.cx, cam.cy);
    }
    let half_w = sw * 0.5;
    let half_h = sh * 0.5;
    (
        ((cam.cx * cam.zoom - half_w).round() + half_w) / cam.zoom,
        ((cam.cy * cam.zoom - half_h).round() + half_h) / cam.zoom,
    )
}

// Build the perspective 3D camera. It floats on the -z side of the canvas,
// looking toward +z, at a distance chosen so that at the z=0 plane `zoom` screen
// pixels map to one canvas unit (keeping the 2D zoom controls meaningful).
// Viewing from -z with up = -y makes world x run rightward and y downward on
// screen, matching canvas coordinates directly (no mirror). Windows lift toward
// the viewer along -z.
pub fn camera_3d(cam: &Camera, screen_w: i32, screen_h: i32, set: &Settings) -> ray::Camera3D {
    // The aligned centre, which is where the view is built from and therefore also what every
    // screen to canvas projection goes through, so a hit test agrees with what was drawn.
    let (cx, cy) = snapped_center(cam, screen_w as f32, screen_h as f32);
    let half = (set.fov_deg * 0.5).to_radians();
    let dist = screen_h as f32 / (2.0 * cam.zoom * half.tan());
    ray::Camera3D {
        position: ray::Vector3 { x: cx, y: cy, z: -dist },
        target: ray::Vector3 { x: cx, y: cy, z: 0.0 },
        up: ray::Vector3 { x: 0.0, y: -1.0, z: 0.0 },
        fovy: set.fov_deg,
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

