//
// Render (Data Oriented zone)
//
// Owns the window texture store, the dmabuf import cache, and the draw pass.
//
// shm surfaces are uploaded into GL textures via CPU copy (owned per window,
// updated in place). dmabuf surfaces are imported zero copy as EGLImage backed
// textures, cached per client buffer so a swapchain's buffers are imported once
// and reused as they cycle (no per-frame create/destroy churn). Each toplevel
// is drawn as a shader-processed quad, centered at native size.
//

use crate::camera;
use crate::egl::{Egl, EglImage};
use crate::ray::{self, Shader, Vector3};
use crate::wl::state::{self, State};
use smithay::reexports::wayland_server::backend::ObjectId;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::Resource;

//
// Constants
//

// How high (canvas units) a window rises while lifted, and how fast z animates.
const LIFT_HEIGHT: f32 = 250.0;
const LIFT_RATE: f32 = 14.0;

//
// Types
//

// One logical window per index. Position is always on the z=0 plane
// (canvas_x/y); `z` is a purely visual lift toward the camera.
pub struct Windows {
    pub surface: Vec<WlSurface>,
    pub tex_id: Vec<u32>,
    pub tex_w: Vec<i32>,
    pub tex_h: Vec<i32>,
    // Top-left position on the infinite canvas, assigned on first appearance.
    pub canvas_x: Vec<f32>,
    pub canvas_y: Vec<f32>,
    // Visual lift height (current + target, animated) and stack order for
    // draw-order ties among windows at the same z.
    pub z: Vec<f32>,
    pub target_z: Vec<f32>,
    pub order: Vec<u32>,
    // 1.0 for shm (BGRA in memory), 0.0 for dmabuf (correct RGBA via EGL).
    pub swizzle: Vec<f32>,
    // true when we own tex_id (shm, freed via unload). false for dmabuf
    // textures, which the cache owns.
    pub owns: Vec<bool>,
    // Running x cursor for laying new windows out in a row without overlap.
    place_x: f32,
    // Next stack order to assign (monotonic; higher = more recently raised).
    next_order: u32,
    // Reused scratch for repacking shm rows when stride != width*4.
    scratch: Vec<u8>,
}

// dmabuf EGLImage cache keyed by the client's wl_buffer identity.
pub struct DmabufCache {
    pub key: Vec<ObjectId>,
    pub image: Vec<EglImage>,
    pub tex: Vec<u32>,
    pub w: Vec<i32>,
    pub h: Vec<i32>,
    logged: bool,
}

//
// Store construction
//

pub fn windows_new() -> Windows {
    Windows {
        surface: Vec::new(),
        tex_id: Vec::new(),
        tex_w: Vec::new(),
        tex_h: Vec::new(),
        canvas_x: Vec::new(),
        canvas_y: Vec::new(),
        z: Vec::new(),
        target_z: Vec::new(),
        order: Vec::new(),
        swizzle: Vec::new(),
        owns: Vec::new(),
        place_x: 0.0,
        next_order: 0,
        scratch: Vec::new(),
    }
}

pub fn dmabuf_cache_new() -> DmabufCache {
    DmabufCache {
        key: Vec::new(),
        image: Vec::new(),
        tex: Vec::new(),
        w: Vec::new(),
        h: Vec::new(),
        logged: false,
    }
}

fn index_of(windows: &Windows, surface: &WlSurface) -> Option<usize> {
    windows.surface.iter().position(|s| s == surface)
}

// Remove windows whose surface has died (client closed), freeing any shm
// texture we own. dmabuf textures belong to the cache and its buffer_destroyed
// path, so we do not touch them here.
pub fn prune_dead(windows: &mut Windows) {
    let mut i = 0;
    while i < windows.surface.len() {
        if windows.surface[i].is_alive() {
            i += 1;
            continue;
        }
        if windows.owns[i] && windows.tex_id[i] != 0 {
            ray::unload_texture(windows.tex_id[i]);
        }
        windows.surface.remove(i);
        windows.tex_id.remove(i);
        windows.tex_w.remove(i);
        windows.tex_h.remove(i);
        windows.canvas_x.remove(i);
        windows.canvas_y.remove(i);
        windows.z.remove(i);
        windows.target_z.remove(i);
        windows.order.remove(i);
        windows.swizzle.remove(i);
        windows.owns.remove(i);
    }
}

// Topmost window under the cursor: cast the cursor ray to the z=0 plane and pick
// the highest-order window whose rect contains the hit. Returns the surface and
// its canvas origin.
pub fn window_at(
    windows: &Windows,
    cam3d: ray::Camera3D,
    sx: f32,
    sy: f32,
) -> Option<(WlSurface, f32, f32)> {
    let (cx, cy) = camera::screen_to_plane(cam3d, sx, sy, 0.0)?;
    let mut best: Option<usize> = None;
    for i in 0..windows.surface.len() {
        let x = windows.canvas_x[i];
        let y = windows.canvas_y[i];
        let w = windows.tex_w[i] as f32;
        let h = windows.tex_h[i] as f32;
        if cx >= x && cx < x + w && cy >= y && cy < y + h {
            if best.map_or(true, |b| windows.order[i] > windows.order[b]) {
                best = Some(i);
            }
        }
    }
    best.map(|i| (windows.surface[i].clone(), windows.canvas_x[i], windows.canvas_y[i]))
}

// Canvas origin of a specific surface, if present.
pub fn window_origin(windows: &Windows, surface: &WlSurface) -> Option<(f32, f32)> {
    index_of(windows, surface).map(|i| (windows.canvas_x[i], windows.canvas_y[i]))
}

// Move a window to a canvas position.
pub fn set_window_pos(windows: &mut Windows, surface: &WlSurface, x: f32, y: f32) {
    if let Some(i) = index_of(windows, surface) {
        windows.canvas_x[i] = x;
        windows.canvas_y[i] = y;
    }
}

// Bring a window to the front of the stack (higher order draws on top).
pub fn front(windows: &mut Windows, surface: &WlSurface) {
    if let Some(i) = index_of(windows, surface) {
        windows.order[i] = windows.next_order;
        windows.next_order += 1;
    }
}

// Front + lift toward the camera (grab). The camera views from -z, so lifting
// toward the viewer means moving to negative z.
pub fn raise(windows: &mut Windows, surface: &WlSurface) {
    front(windows, surface);
    if let Some(i) = index_of(windows, surface) {
        windows.target_z[i] = -LIFT_HEIGHT;
    }
}

// Settle a window back onto the plane, keeping its stack order (drop).
pub fn settle(windows: &mut Windows, surface: &WlSurface) {
    if let Some(i) = index_of(windows, surface) {
        windows.target_z[i] = 0.0;
    }
}

// Animate every window's lift toward its target.
pub fn animate(windows: &mut Windows, dt: f32) {
    let t = (LIFT_RATE * dt).min(1.0);
    for i in 0..windows.z.len() {
        windows.z[i] += (windows.target_z[i] - windows.z[i]) * t;
    }
}

fn store_entry(
    windows: &mut Windows,
    surface: &WlSurface,
    tex_id: u32,
    w: i32,
    h: i32,
    swizzle: f32,
    owns: bool,
) {
    match index_of(windows, surface) {
        Some(i) => {
            // Only free the previous texture if we owned it (shm). dmabuf
            // textures belong to the cache.
            if windows.owns[i] && windows.tex_id[i] != 0 {
                ray::unload_texture(windows.tex_id[i]);
            }
            windows.tex_id[i] = tex_id;
            windows.tex_w[i] = w;
            windows.tex_h[i] = h;
            windows.swizzle[i] = swizzle;
            windows.owns[i] = owns;
        }
        None => {
            // Lay windows out left to right, no overlap, sized to each window.
            const GAP: f32 = 80.0;
            let cx = windows.place_x;
            let cy = 0.0;
            windows.place_x += w as f32 + GAP;
            let order = windows.next_order;
            windows.next_order += 1;
            windows.surface.push(surface.clone());
            windows.tex_id.push(tex_id);
            windows.tex_w.push(w);
            windows.tex_h.push(h);
            windows.canvas_x.push(cx);
            windows.canvas_y.push(cy);
            windows.z.push(0.0);
            windows.target_z.push(0.0);
            windows.order.push(order);
            windows.swizzle.push(swizzle);
            windows.owns.push(owns);
        }
    }
}

//
// dmabuf cache
//

impl DmabufCache {
    fn index_of(&self, key: &ObjectId) -> Option<usize> {
        self.key.iter().position(|k| k == key)
    }

    // Reuse the cached texture for this buffer, importing it on first sight.
    fn get_or_import(
        &mut self,
        egl: &Egl,
        key: ObjectId,
        info: &crate::egl::DmabufInfo,
    ) -> Option<(u32, i32, i32)> {
        if let Some(i) = self.index_of(&key) {
            return Some((self.tex[i], self.w[i], self.h[i]));
        }

        let (image, tex) = egl.import_dmabuf(info)?;
        if !self.logged {
            self.logged = true;
            println!(
                "om_wm: ZERO-COPY dmabuf import ok {}x{} fourcc={:#x} mod={:#x} gl_tex={tex}",
                info.width, info.height, info.fourcc, info.modifier
            );
        }
        self.key.push(key);
        self.image.push(image);
        self.tex.push(tex);
        self.w.push(info.width);
        self.h.push(info.height);
        Some((tex, info.width, info.height))
    }

    pub fn evict(&mut self, egl: &Egl, key: &ObjectId) {
        if let Some(i) = self.index_of(key) {
            egl.destroy(self.image[i], self.tex[i]);
            self.key.swap_remove(i);
            self.image.swap_remove(i);
            self.tex.swap_remove(i);
            self.w.swap_remove(i);
            self.h.swap_remove(i);
        }
    }

    pub fn destroy_all(&mut self, egl: &Egl) {
        for i in 0..self.tex.len() {
            egl.destroy(self.image[i], self.tex[i]);
        }
        self.key.clear();
        self.image.clear();
        self.tex.clear();
        self.w.clear();
        self.h.clear();
    }
}

//
// Import
//

pub fn upload_committed(
    windows: &mut Windows,
    cache: &mut DmabufCache,
    egl: &Egl,
    state: &mut State,
    surface: &WlSurface,
) {
    let handled_shm = state::take_shm_buffer(surface, |w, h, stride, ptr| {
        upload_shm(windows, surface, w, h, stride, ptr);
    });
    if handled_shm {
        // Surface is on an shm buffer now; drop any dmabuf we were holding.
        state::release_held_dmabuf(state, surface);
        return;
    }

    state::take_dmabuf_and_retain(state, surface, |key, info| {
        match cache.get_or_import(egl, key, info) {
            Some((tex, w, h)) => {
                store_entry(windows, surface, tex, w, h, 0.0, false);
            }
            None => eprintln!(
                "om_wm: dmabuf import failed {}x{} fourcc={:#x} mod={:#x}",
                info.width, info.height, info.fourcc, info.modifier
            ),
        }
    });
}

fn upload_shm(
    windows: &mut Windows,
    surface: &WlSurface,
    w: i32,
    h: i32,
    stride: i32,
    ptr: *const u8,
) {
    if w <= 0 || h <= 0 {
        return;
    }

    // Ensure tightly packed rows for rlgl (no stride parameter).
    let data: *const u8 = if stride == w * 4 {
        ptr
    } else {
        let row = (w * 4) as usize;
        windows.scratch.clear();
        windows.scratch.reserve(row * h as usize);
        for y in 0..h as usize {
            let src = unsafe { ptr.add(y * stride as usize) };
            let slice = unsafe { std::slice::from_raw_parts(src, row) };
            windows.scratch.extend_from_slice(slice);
        }
        windows.scratch.as_ptr()
    };

    // Fast path: reuse an existing same-size shm texture in place.
    if let Some(i) = index_of(windows, surface) {
        if windows.owns[i] && windows.tex_w[i] == w && windows.tex_h[i] == h {
            ray::update_texture_rgba(windows.tex_id[i], data, w, h);
            windows.swizzle[i] = 1.0;
            return;
        }
    }

    let id = ray::load_texture_rgba(data, w, h);
    store_entry(windows, surface, id, w, h, 1.0, true);
}

// Release the shm textures we own. dmabuf textures live in the cache.
pub fn destroy_owned(windows: &mut Windows) {
    for i in 0..windows.tex_id.len() {
        if windows.owns[i] && windows.tex_id[i] != 0 {
            ray::unload_texture(windows.tex_id[i]);
        }
    }
    windows.surface.clear();
    windows.tex_id.clear();
    windows.tex_w.clear();
    windows.tex_h.clear();
    windows.canvas_x.clear();
    windows.canvas_y.clear();
    windows.z.clear();
    windows.target_z.clear();
    windows.order.clear();
    windows.swizzle.clear();
    windows.owns.clear();
    windows.place_x = 0.0;
}

//
// Draw
//

pub fn draw_toplevels(
    windows: &Windows,
    cam3d: ray::Camera3D,
    shader: Shader,
    alpha_loc: i32,
    swizzle_loc: i32,
) {
    // Painter's order: the camera is on the -z side, so draw far (high z) first,
    // near (low z) last; ties broken by stack order. Depth test is off so later
    // draws win among equal z.
    let mut idx: Vec<usize> = (0..windows.surface.len())
        .filter(|&i| windows.tex_w[i] > 0 && windows.tex_h[i] > 0)
        .collect();
    idx.sort_by(|&a, &b| {
        windows.z[b]
            .partial_cmp(&windows.z[a])
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(windows.order[a].cmp(&windows.order[b]))
    });

    ray::begin_mode_3d(cam3d);
    ray::disable_backface_culling();
    ray::disable_depth_test();
    for i in idx {
        let x = windows.canvas_x[i];
        let y = windows.canvas_y[i];
        let w = windows.tex_w[i] as f32;
        let h = windows.tex_h[i] as f32;
        let z = windows.z[i];
        // Quad on a plane parallel to the canvas, raised by z. Top-left origin.
        let corners = [
            (Vector3 { x, y, z }, 0.0, 0.0),
            (Vector3 { x: x + w, y, z }, 1.0, 0.0),
            (Vector3 { x: x + w, y: y + h, z }, 1.0, 1.0),
            (Vector3 { x, y: y + h, z }, 0.0, 1.0),
        ];
        ray::begin_shader_mode(shader);
        ray::set_shader_float(shader, alpha_loc, 0.0);
        ray::set_shader_float(shader, swizzle_loc, windows.swizzle[i]);
        ray::draw_textured_quad(windows.tex_id[i], corners);
        ray::end_shader_mode();
    }
    ray::enable_depth_test();
    ray::enable_backface_culling();
    ray::end_mode_3d();
}
