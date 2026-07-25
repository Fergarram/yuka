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

use crate::camera::{self, Camera};
use crate::egl::{Egl, EglImage};
use crate::ray::{self, Rectangle, Shader, Texture2D};
use crate::wl::state::{self, State};
use smithay::reexports::wayland_server::backend::ObjectId;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::Resource;

//
// Types
//

// One logical window per index.
pub struct Windows {
    pub surface: Vec<WlSurface>,
    pub tex_id: Vec<u32>,
    pub tex_w: Vec<i32>,
    pub tex_h: Vec<i32>,
    // Top-left position on the infinite canvas, assigned on first appearance.
    pub canvas_x: Vec<f32>,
    pub canvas_y: Vec<f32>,
    // 1.0 for shm (BGRA in memory), 0.0 for dmabuf (correct RGBA via EGL).
    pub swizzle: Vec<f32>,
    // true when we own tex_id (shm, freed via unload). false for dmabuf
    // textures, which the cache owns.
    pub owns: Vec<bool>,
    // Running x cursor for laying new windows out in a row without overlap.
    place_x: f32,
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
        swizzle: Vec::new(),
        owns: Vec::new(),
        place_x: 0.0,
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
        windows.swizzle.remove(i);
        windows.owns.remove(i);
    }
}

// Topmost window whose canvas rect contains the point; returns the surface and
// its canvas origin. Later entries are considered on top.
pub fn window_at(
    windows: &Windows,
    cx: f32,
    cy: f32,
) -> Option<(WlSurface, f32, f32)> {
    for i in (0..windows.surface.len()).rev() {
        let x = windows.canvas_x[i];
        let y = windows.canvas_y[i];
        let w = windows.tex_w[i] as f32;
        let h = windows.tex_h[i] as f32;
        if cx >= x && cx < x + w && cy >= y && cy < y + h {
            return Some((windows.surface[i].clone(), x, y));
        }
    }
    None
}

// Canvas origin of a specific surface, if present.
pub fn window_origin(windows: &Windows, surface: &WlSurface) -> Option<(f32, f32)> {
    index_of(windows, surface).map(|i| (windows.canvas_x[i], windows.canvas_y[i]))
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
            windows.surface.push(surface.clone());
            windows.tex_id.push(tex_id);
            windows.tex_w.push(w);
            windows.tex_h.push(h);
            windows.canvas_x.push(cx);
            windows.canvas_y.push(cy);
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
    windows.swizzle.clear();
    windows.owns.clear();
    windows.place_x = 0.0;
}

//
// Draw
//

pub fn draw_toplevels(
    windows: &Windows,
    state: &State,
    cam: &Camera,
    shader: Shader,
    alpha_loc: i32,
    swizzle_loc: i32,
) {
    let screen_w = ray::screen_width();
    let screen_h = ray::screen_height();

    for surface in state::toplevel_surfaces(state) {
        let Some(i) = index_of(windows, &surface) else {
            continue;
        };
        let tw = windows.tex_w[i];
        let th = windows.tex_h[i];
        if tw <= 0 || th <= 0 {
            continue;
        }

        let texture = Texture2D {
            id: windows.tex_id[i],
            width: tw,
            height: th,
            mipmaps: 1,
            format: ray::PIXELFORMAT_R8G8B8A8,
        };
        // Both shm uploads and dmabuf EGLImages keep the buffer's top-left
        // origin, so no vertical flip is needed.
        let source = Rectangle { x: 0.0, y: 0.0, width: tw as f32, height: th as f32 };

        // Place the window on the canvas, mapped through the camera. The buffer
        // is (tw, th) canvas units; zoom scales it to screen pixels.
        let (sx, sy) = camera::canvas_to_screen(
            cam, screen_w, screen_h, windows.canvas_x[i], windows.canvas_y[i],
        );
        // Snap to integer pixels so windows stay crisp at fractional zoom.
        let dest = Rectangle {
            x: sx.round(),
            y: sy.round(),
            width: (tw as f32 * cam.zoom).round(),
            height: (th as f32 * cam.zoom).round(),
        };

        ray::begin_shader_mode(shader);
        ray::set_shader_float(shader, alpha_loc, 0.0);
        ray::set_shader_float(shader, swizzle_loc, windows.swizzle[i]);
        ray::draw_texture_pro(texture, source, dest, ray::WHITE);
        ray::end_shader_mode();
    }
}
