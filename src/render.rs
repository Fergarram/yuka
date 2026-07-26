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
const LIFT_HEIGHT: f32 = 125.0;
const LIFT_RATE: f32 = 14.0;
// How far in front of its parent a child surface sits: enough to win the painter's
// sort, small enough to read as the same plane. Negative z is toward the camera.
const CHILD_LIFT: f32 = 0.05;

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
    // The window geometry rectangle inside the surface: offset and size. This, not
    // the whole surface, is what we draw and what we hit test, so a client's drop
    // shadow is neither visible nor clickable. Falls back to the full texture when
    // a client never set a geometry.
    pub geo_x: Vec<f32>,
    pub geo_y: Vec<f32>,
    pub geo_w: Vec<f32>,
    pub geo_h: Vec<f32>,
    // 1.0 for shm (BGRA in memory), 0.0 for dmabuf (correct RGBA via EGL).
    pub swizzle: Vec<f32>,
    // true when we own tex_id (shm, freed via unload). false for dmabuf
    // textures, which the cache owns.
    pub owns: Vec<bool>,
    // true for popups (menus). Canvas content like any window, positioned from
    // their parent every frame and sitting a hair in front of it, so a menu scales
    // and pans with the window it belongs to.
    pub popup: Vec<bool>,
    // true for subsurfaces: content a client attaches beside its main surface. Same
    // deal as popups, positioned from the root every frame, in front of it in tree
    // order.
    pub sub: Vec<bool>,
    // Whether sync_popups placed this popup on the current frame. A popup that has
    // left its parent's tree but whose surface is not destroyed yet would
    // otherwise keep its last rectangle, drawing stale and swallowing clicks.
    pub placed: Vec<bool>,
    // Where new windows go: the canvas point at the middle of the view, which the
    // main loop keeps current, plus a cascade step so a second window does not
    // land exactly on the first. A row from the origin was fine for a fixed test
    // set and wrong for real apps: anything that opened later, including a
    // browser's own dialogs, landed off screen and looked like it never appeared.
    place_at: (f32, f32),
    cascade: u32,
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
        geo_x: Vec::new(),
        geo_y: Vec::new(),
        geo_w: Vec::new(),
        geo_h: Vec::new(),
        swizzle: Vec::new(),
        owns: Vec::new(),
        popup: Vec::new(),
        sub: Vec::new(),
        placed: Vec::new(),
        place_at: (0.0, 0.0),
        cascade: 0,
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

// The part of an entry that is actually the window, in canvas coordinates: its
// surface origin shifted by the geometry offset, sized to the geometry. Everything
// visible or clickable is expressed in these, never in raw texture bounds.
pub fn visible(windows: &Windows, i: usize) -> (f32, f32, f32, f32) {
    (
        windows.canvas_x[i] + windows.geo_x[i],
        windows.canvas_y[i] + windows.geo_y[i],
        windows.geo_w[i],
        windows.geo_h[i],
    )
}

// Record what the client called its window geometry, clamped to the texture we
// have, since a stale configure can describe a size the current buffer does not.
pub fn set_geometry(windows: &mut Windows, surface: &WlSurface, geo: Option<(f32, f32, f32, f32)>) {
    let Some(i) = index_of(windows, surface) else { return };
    let tw = windows.tex_w[i] as f32;
    let th = windows.tex_h[i] as f32;
    let (x, y, w, h) = geo.unwrap_or((0.0, 0.0, tw, th));
    windows.geo_x[i] = x.clamp(0.0, tw);
    windows.geo_y[i] = y.clamp(0.0, th);
    windows.geo_w[i] = w.min(tw - windows.geo_x[i]).max(1.0);
    windows.geo_h[i] = h.min(th - windows.geo_y[i]).max(1.0);
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
        windows.geo_x.remove(i);
        windows.geo_y.remove(i);
        windows.geo_w.remove(i);
        windows.geo_h.remove(i);
        windows.swizzle.remove(i);
        windows.owns.remove(i);
        windows.popup.remove(i);
        windows.sub.remove(i);
        windows.placed.remove(i);
    }
}

// A stored entry is drawable when it has a texture and, if it is a child, when its
// parent placed it this frame.
fn drawable(windows: &Windows, i: usize) -> bool {
    if windows.tex_w[i] <= 0 || windows.tex_h[i] <= 0 {
        return false;
    }
    !child(windows, i) || windows.placed[i]
}

// Popups and subsurfaces are children: their position comes from a parent each
// frame, they are never laid out on the canvas themselves, and they are never
// dragged.
pub fn child(windows: &Windows, i: usize) -> bool {
    windows.popup[i] || windows.sub[i]
}

// Topmost surface under the cursor: cast the ray to the z=0 plane and pick the
// nearest candidate containing the hit. Nearest means smallest z, since children
// sit in front of their parents, with stack order breaking ties.
pub fn window_at(
    windows: &Windows,
    cam3d: ray::Camera3D,
    sx: f32,
    sy: f32,
) -> Option<(WlSurface, f32, f32)> {
    let (cx, cy) = camera::screen_to_plane(cam3d, sx, sy, 0.0)?;
    let mut best: Option<usize> = None;
    for i in 0..windows.surface.len() {
        if !drawable(windows, i) {
            continue;
        }
        let (x, y, w, h) = visible(windows, i);
        if cx < x || cx >= x + w || cy < y || cy >= y + h {
            continue;
        }
        // A client can declare an input region smaller than its buffer; a
        // transparent surface without one would otherwise swallow clicks meant for
        // whatever is behind it.
        let local_x = cx - windows.canvas_x[i];
        let local_y = cy - windows.canvas_y[i];
        if state::input_region_contains(&windows.surface[i], local_x, local_y) {
            let nearer = |b: usize| {
                windows.z[i] < windows.z[b]
                    || (windows.z[i] == windows.z[b]
                        && windows.order[i] >= windows.order[b])
            };
            if best.map_or(true, nearer) {
                best = Some(i);
            }
        }
    }
    best.map(|i| (windows.surface[i].clone(), windows.canvas_x[i], windows.canvas_y[i]))
}

// Canvas origin of a surface's own coordinate space (its top-left, geometry
// offset included), for following a surface that is holding a pointer grab while
// it moves.
pub fn surface_origin(windows: &Windows, surface: &WlSurface) -> Option<(f32, f32)> {
    index_of(windows, surface).map(|i| (windows.canvas_x[i], windows.canvas_y[i]))
}

// Current lifted z of a specific surface, if present.
pub fn window_z(windows: &Windows, surface: &WlSurface) -> Option<f32> {
    index_of(windows, surface).map(|i| windows.z[i])
}

// Canvas position of a window's center (origin + half its texture size).
pub fn window_center(windows: &Windows, surface: &WlSurface) -> Option<(f32, f32)> {
    index_of(windows, surface).map(|i| {
        let (x, y, w, h) = visible(windows, i);
        (x + w * 0.5, y + h * 0.5)
    })
}

// Place a window so its center sits at the given canvas position.
pub fn set_window_center(windows: &mut Windows, surface: &WlSurface, cx: f32, cy: f32) {
    if let Some(i) = index_of(windows, surface) {
        windows.canvas_x[i] = cx - windows.geo_w[i] * 0.5 - windows.geo_x[i];
        windows.canvas_y[i] = cy - windows.geo_h[i] * 0.5 - windows.geo_y[i];
    }
}

// Move a window to a canvas position.
pub fn set_window_pos(windows: &mut Windows, surface: &WlSurface, x: f32, y: f32) {
    if let Some(i) = index_of(windows, surface) {
        windows.canvas_x[i] = x;
        windows.canvas_y[i] = y;
    }
}

// Dump every rect the hit test can pick, for when a click seems to land inside
// something and does not.
pub fn log_rects(windows: &Windows) {
    for i in 0..windows.surface.len() {
        println!(
            "om_wm:   {} {}x{} at {:.0},{:.0} z={:.2} order={} placed={}",
            if windows.popup[i] {
                "popup "
            } else if windows.sub[i] {
                "sub   "
            } else {
                "window"
            },
            windows.tex_w[i],
            windows.tex_h[i],
            windows.canvas_x[i],
            windows.canvas_y[i],
            windows.z[i],
            windows.order[i],
            windows.placed[i]
        );
    }
}

// Where the next new window will be centred: the canvas point in the middle of
// the view. Kept current by the main loop so a window always opens where you are
// looking, however far the canvas has been panned.
pub fn set_place_origin(windows: &mut Windows, cx: f32, cy: f32) {
    windows.place_at = (cx, cy);
}

// Anchor every popup to its parent on the canvas. The offset is the position the
// client acked for its positioner, in the parent's surface coordinates, so the
// menu lands where the client asked, and it scales and pans with the window it
// belongs to. It sits CHILD_LIFT in front of the parent, which wins the painter's
// sort without leaving the parent's plane and stacks submenus naturally: each
// level is one step nearer the camera.
//
// Called once a frame, before anything is hit tested or drawn. A child that is no
// longer in any parent's tree stays unplaced, and unplaced children neither draw
// nor accept clicks.
pub fn sync_children(windows: &mut Windows) {
    for i in 0..windows.surface.len() {
        windows.placed[i] = false;
    }
    let mut moves: Vec<(usize, f32, f32, f32, u32)> = Vec::new();
    for i in 0..windows.surface.len() {
        if windows.popup[i] {
            continue;
        }
        for (popup, ox, oy) in state::popups_of(&windows.surface[i]) {
            if let Some(j) = index_of(windows, &popup) {
                moves.push((
                    j,
                    windows.canvas_x[i] + ox,
                    windows.canvas_y[i] + oy,
                    windows.z[i] - CHILD_LIFT,
                    windows.order[i],
                ));
            }
        }
    }
    for (j, x, y, z, order) in moves {
        windows.canvas_x[j] = x;
        windows.canvas_y[j] = y;
        windows.z[j] = z;
        windows.target_z[j] = z;
        windows.order[j] = order;
        windows.placed[j] = true;
    }

    // Then every subsurface tree, from each root that is not itself a subsurface, so
    // a popup's children are placed off the popup we just moved. The tree comes back
    // in the client's stacking order with the root at its own slot, and each surface
    // is offset from the root's z by its distance from that slot: earlier slots are
    // behind the root, later ones in front. Forcing them all in front, which is what
    // this did first, puts a client's below-parent shadow on top of its own content
    // and swallows every click aimed at it.
    let mut subs: Vec<(usize, f32, f32, f32, u32)> = Vec::new();
    for i in 0..windows.surface.len() {
        if windows.sub[i] {
            continue;
        }
        let tree = state::surface_tree(&windows.surface[i]);
        let root_slot = tree
            .iter()
            .position(|(surface, _, _)| *surface == windows.surface[i])
            .unwrap_or(0);
        for (slot, (surface, ox, oy)) in tree.iter().enumerate() {
            if slot == root_slot {
                continue;
            }
            let Some(j) = index_of(windows, surface) else { continue };
            let steps = root_slot as f32 - slot as f32;
            subs.push((
                j,
                windows.canvas_x[i] + ox,
                windows.canvas_y[i] + oy,
                windows.z[i] + CHILD_LIFT * steps,
                windows.order[i],
            ));
        }
    }
    for (j, x, y, z, order) in subs {
        windows.canvas_x[j] = x;
        windows.canvas_y[j] = y;
        windows.z[j] = z;
        windows.target_z[j] = z;
        windows.order[j] = order;
        windows.placed[j] = true;
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
    popup: bool,
    sub: bool,
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
            // Centre new windows on the view, stepped so successive ones do not
            // hide each other. Popups are not placed at all: sync_popups puts them
            // under their parent before the first draw.
            const CASCADE_STEP: f32 = 48.0;
            const CASCADE_WRAP: u32 = 6;
            let (cx, cy) = if popup || sub {
                (0.0, 0.0)
            } else {
                let step = (windows.cascade % CASCADE_WRAP) as f32 * CASCADE_STEP;
                windows.cascade += 1;
                (
                    windows.place_at.0 - w as f32 * 0.5 + step,
                    windows.place_at.1 - h as f32 * 0.5 + step,
                )
                // Geometry is not known until the client's first commit lands, so
                // this centres on the buffer and set_geometry refines it after.
            };
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
            windows.geo_x.push(0.0);
            windows.geo_y.push(0.0);
            windows.geo_w.push(w as f32);
            windows.geo_h.push(h as f32);
            windows.swizzle.push(swizzle);
            windows.owns.push(owns);
            windows.popup.push(popup);
            windows.sub.push(sub);
            windows.placed.push(false);
            println!(
                "om_wm: {} + {w}x{h} at {cx:.0},{cy:.0}",
                if popup { "popup" } else { "window" }
            );
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
    // Windows, their popups and their subsurfaces all become quads. Anything else
    // that commits a buffer (a cursor surface, say) is dropped, releasing shm so the
    // client is not left waiting on it.
    let (popup, sub) = if state::is_toplevel(state, surface) {
        (false, false)
    } else if state::is_popup(state, surface) {
        (true, false)
    } else if state::is_subsurface(surface) {
        (false, true)
    } else {
        state::take_shm_buffer(surface, |_, _, _, _| {});
        return;
    };

    let handled_shm = state::take_shm_buffer(surface, |w, h, stride, ptr| {
        upload_shm(windows, surface, w, h, stride, ptr, popup, sub);
    });
    if handled_shm {
        set_geometry(windows, surface, state::geometry_of(surface));
        // Surface is on an shm buffer now; drop any dmabuf we were holding.
        state::release_held_dmabuf(state, surface);
        return;
    }

    state::take_dmabuf_and_retain(state, surface, |key, info| {
        match cache.get_or_import(egl, key, info) {
            Some((tex, w, h)) => {
                store_entry(windows, surface, tex, w, h, 0.0, false, popup, sub);
            }
            None => eprintln!(
                "om_wm: dmabuf import failed {}x{} fourcc={:#x} mod={:#x}",
                info.width, info.height, info.fourcc, info.modifier
            ),
        }
    });
    set_geometry(windows, surface, state::geometry_of(surface));
}

fn upload_shm(
    windows: &mut Windows,
    surface: &WlSurface,
    w: i32,
    h: i32,
    stride: i32,
    ptr: *const u8,
    popup: bool,
    sub: bool,
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
    store_entry(windows, surface, id, w, h, 1.0, true, popup, sub);
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
    windows.geo_x.clear();
    windows.geo_y.clear();
    windows.geo_w.clear();
    windows.geo_h.clear();
    windows.swizzle.clear();
    windows.owns.clear();
    windows.popup.clear();
    windows.sub.clear();
    windows.placed.clear();
    windows.cascade = 0;
}

//
// Draw
//

pub fn draw_windows(
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
        .filter(|&i| drawable(windows, i))
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
        let (x, y, w, h) = visible(windows, i);
        let z = windows.z[i];
        // Texture coordinates cover only the geometry rectangle, so whatever the
        // client padded around its window is cropped rather than drawn.
        let tw = windows.tex_w[i] as f32;
        let th = windows.tex_h[i] as f32;
        let u0 = windows.geo_x[i] / tw;
        let v0 = windows.geo_y[i] / th;
        let u1 = (windows.geo_x[i] + w) / tw;
        let v1 = (windows.geo_y[i] + h) / th;
        // Quad on a plane parallel to the canvas, raised by z. Top-left origin.
        let corners = [
            (Vector3 { x, y, z }, u0, v0),
            (Vector3 { x: x + w, y, z }, u1, v0),
            (Vector3 { x: x + w, y: y + h, z }, u1, v1),
            (Vector3 { x, y: y + h, z }, u0, v1),
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
