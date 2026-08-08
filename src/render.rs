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
// Zero copy is the default and not the only option: see DmabufMode, whose Blit arm copies an
// imported buffer into a texture of ours so it can be released at once and can carry a mip
// chain. What that is for, and what it measured, is in the settings comment.
//

use crate::camera;
use crate::touch;
use crate::egl::{self, Egl, EglImage};
use crate::ray::{self, Shader, Vector3};
use crate::wl::state::{self, Keep, State};
use smithay::reexports::wayland_server::backend::ObjectId;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::Resource;

//
// Settings
//

// Whether a minified window gets a mip chain and trilinear + anisotropic sampling.
// Off for now: plain bilinear when zoomed out, nearest at 1:1 and above. Only a texture we
// own can have a chain at all, so this is uneven across windows (see prepare_textures).
//
// DmabufMode::Blit is the other way out of that: it copies an imported buffer into a texture
// of ours, which can then carry a chain like an shm one. Measured at roughly three times the
// slow frame rate, so it is a zoom quality decision rather than a free one.
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
    // Top-left position on the infinite canvas, assigned on first appearance. This is the
    // truth about where a window is, and only something that actually moves the window ever
    // writes it: a drag, a resize that holds an edge still, a child being placed from its
    // parent. Nothing to do with pixels touches it.
    pub canvas_x: Vec<f32>,
    pub canvas_y: Vec<f32>,
    // The same position rounded onto the pixel grid, recomputed from canvas_x/y every frame
    // by align_positions and never fed back into them. Everything visible reads these.
    //
    // Derived rather than stored because the grid depends on the zoom, so the rounding is a
    // property of how the window is being *looked at*, not of where it is. Writing it back
    // was a bug you could watch: at zoom z the grid is 1/z canvas units, a pinch changes z
    // every frame, and each frame re-rounded the previous frame's rounded value against a
    // different grid. Sixty half-pixel corrections a second, each window walking on its own,
    // and at zoom 0.3 two windows a unit apart could land on top of each other for good.
    pub draw_x: Vec<f32>,
    pub draw_y: Vec<f32>,
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
    // Live stretch during a resize drag, and 1.0 the rest of the time, which is every
    // window almost always and every window at all unless resize_stretch is turned on.
    //
    // A resize on Wayland is a request: we ask, the client renders, and its pixels arrive a
    // round trip later. By default we wait for them, so the window is only ever the size the
    // client has committed. With the stretch on, the quad is scaled here to where the cursor
    // is while the client is asked for the same size in parallel, so the corner tracks your
    // hand at the cost of showing content at a size it was not drawn at.
    pub scale_x: Vec<f32>,
    pub scale_y: Vec<f32>,
    // What we have told GL this texture's sampler is, so the choice is only pushed when it
    // actually changes rather than every frame. FILTER_UNSET whenever the texture is new and
    // nothing has been told to it yet, which is the one thing this column must never guess:
    // a guess that happens to match what the next frame wants means the call is skipped and
    // the texture keeps whatever GL gave it.
    pub filter: Vec<u8>,
    // Whether this window is maximized, and the rectangle it had before it was. A maximized
    // window is one that took the shape of the view: the size is the geometry size we asked
    // the client for, the position is the visible top-left we set, and both are what
    // unmaximizing gives back. Meaningless while maximized is false.
    // How opaque this window is drawn right now, and where it is heading. A window covering the
    // focused one fades, and eases rather than snapping: the arrangement changes as you click
    // around and as windows move, and a hard cut on every change reads as flicker.
    pub alpha: Vec<f32>,
    pub alpha_to: Vec<f32>,
    pub maximized: Vec<bool>,
    // And the canvas point its visible top-left is pinned to while it is. Held every frame
    // rather than set once, because the offset between a surface and its visible top-left is
    // the client's to change and it does change on exactly this transition: a client that
    // believes it is maximized drops the invisible shadow border it was carrying, so the
    // geometry offset we positioned through is not the one it commits with.
    pub max_x: Vec<f32>,
    pub max_y: Vec<f32>,
    pub restore_x: Vec<f32>,
    pub restore_y: Vec<f32>,
    pub restore_w: Vec<f32>,
    pub restore_h: Vec<f32>,
    // Sampling state of the texture: MIP_NONE until a window is minified enough to
    // want a mip chain, MIP_READY once it has one, MIP_REFUSED for a texture the
    // driver will not build one for (see egl::build_mips). Reset to MIP_NONE on every
    // commit, because new content makes any existing chain stale.
    pub mip: Vec<u8>,
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
    // How a new window places itself. Policy from the settings, kept here so that placing a window
    // needs nothing but the window list.
    spawn: Spawn,
    place_at: (f32, f32),
    cascade: u32,
    // Next stack order to assign (monotonic; higher = more recently raised).
    next_order: u32,
    // Reused scratch for repacking shm rows when stride != width*4.
    scratch: Vec<u8>,
    // Windows that became real this frame: a toplevel's first buffer, which is the moment it
    // has a size and something to show and can therefore be focused. Drained by the main
    // loop. Popups and subsurfaces are not in here; neither is anything a client has merely
    // created and not yet drawn into.
    pub mapped: Vec<WlSurface>,
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
        draw_x: Vec::new(),
        draw_y: Vec::new(),
        z: Vec::new(),
        target_z: Vec::new(),
        order: Vec::new(),
        geo_x: Vec::new(),
        geo_y: Vec::new(),
        geo_w: Vec::new(),
        geo_h: Vec::new(),
        swizzle: Vec::new(),
        scale_x: Vec::new(),
        scale_y: Vec::new(),
        filter: Vec::new(),
        alpha: Vec::new(),
        alpha_to: Vec::new(),
        maximized: Vec::new(),
        max_x: Vec::new(),
        max_y: Vec::new(),
        restore_x: Vec::new(),
        restore_y: Vec::new(),
        restore_w: Vec::new(),
        restore_h: Vec::new(),
        mip: Vec::new(),
        owns: Vec::new(),
        popup: Vec::new(),
        sub: Vec::new(),
        placed: Vec::new(),
        spawn: Spawn { clear: true, gap: 24.0, order: [DIR_NONE; 4] },
        place_at: (0.0, 0.0),
        cascade: 0,
        next_order: 0,
        scratch: Vec::new(),
        mapped: Vec::new(),
    }
}

// Values for the mip column.
pub const MIP_NONE: u8 = 0;
pub const MIP_READY: u8 = 1;
pub const MIP_REFUSED: u8 = 2;

// Values for the filter column.
pub const FILTER_LINEAR: u8 = 0;
pub const FILTER_NEAREST: u8 = 1;
pub const FILTER_TRILINEAR: u8 = 2;
// A texture we have not set the filter on yet, which is every texture the moment we adopt it.
// This column is a record of what we told GL, and prepare_textures skips a window whose record
// already matches what it wants, so recording a guess is the same as never setting it: raylib
// creates textures on GL_NEAREST, and a window that took a new buffer while the zoom happened
// to want bilinear kept sampling nearest until something else changed its mind. A buffer swap
// is a new texture with its own state, so the honest record is that we know nothing about it.
pub const FILTER_UNSET: u8 = 255;

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
// Reads the pixel-aligned position, not the true one, so that what is hit tested is exactly
// what was drawn. The two differ by less than half a screen pixel.
pub fn visible(windows: &Windows, i: usize) -> (f32, f32, f32, f32) {
    // The stretch is anchored at the top left, which is where a resize drag holds the
    // window while its far corner follows the cursor.
    (
        windows.draw_x[i] + windows.geo_x[i],
        windows.draw_y[i] + windows.geo_y[i],
        windows.geo_w[i] * windows.scale_x[i],
        windows.geo_h[i] * windows.scale_y[i],
    )
}

// A window's drawn rectangle in canvas units, for a caller that has a surface rather than an index.
// The same rectangle the hit test uses, so what is judged to be on screen is what is on screen.
pub fn window_rect(windows: &Windows, surface: &WlSurface) -> Option<(f32, f32, f32, f32)> {
    index_of(windows, surface).map(|i| visible(windows, i))
}

// Which way a new window prefers to look for room, as indices into this list. 255 is "no more
// preferences", so an order may be shorter than four.
pub const DIR_RIGHT: u8 = 0;
pub const DIR_UP: u8 = 1;
pub const DIR_LEFT: u8 = 2;
pub const DIR_DOWN: u8 = 3;
pub const DIR_NONE: u8 = 255;

// How a new window places itself: whether to look for room at all, how much of it to leave, and
// which directions to try before others.
//
// One value rather than three arguments, because it travels together and is set in one place.
#[derive(Clone, Copy)]
pub struct Spawn {
    pub clear: bool,
    pub gap: f32,
    pub order: [u8; 4],
}

// What we do with a client's dmabuf once it is imported. The arm not selected at runtime is
// unused by construction.
//
// There was a third, which sampled in place and handed the buffer straight back so that the
// cost of holding could be measured against something. It tore by construction, and the
// measurement it existed for came back empty: releasing early made no difference to how long
// a client took to answer, so there was nothing to trade the tearing for.
#[allow(dead_code)]
#[derive(Clone, Copy, PartialEq)]
pub enum DmabufMode {
    // Sample the client's buffer in place and keep it until a newer one replaces it. No
    // copy, and the client cannot draw into that buffer while we have it.
    Hold,
    // Copy the imported image into a texture of our own, then hand the buffer back at once.
    // One GPU side copy per commit, and in exchange the client never waits on us, nothing
    // tears, and the texture is ours so it can carry a mip chain.
    Blit,
}

// What a window's pixels came from, for the debug traces: a texture we uploaded out of an
// shm buffer, or a client dmabuf we imported. Which one it is decides whether the client is
// ever waiting on us for a buffer, so a trace about client latency has to say.
// A copied dmabuf is owned like an shm texture, so ownership alone no longer names the path.
// The swizzle does: shm is BGRA in memory and wants it, an imported image and a copy of one
// are both already RGBA.
pub fn source(windows: &Windows, surface: &WlSurface) -> &'static str {
    match index_of(windows, surface) {
        Some(i) if !windows.owns[i] => "dmabuf",
        Some(i) if windows.swizzle[i] > 0.5 => "shm",
        Some(_) => "blit",
        None => "gone",
    }
}

// The window's real size, with no stretch applied: what the client last committed, and
// therefore what a resize measures from and watches for a change in.
pub fn geo_size(windows: &Windows, surface: &WlSurface) -> Option<(f32, f32)> {
    index_of(windows, surface).map(|i| (windows.geo_w[i], windows.geo_h[i]))
}

// Stretch a window without telling it. Only for the duration of a resize drag: the
// texture is the old size, so the content is scaled, and a window left like this would
// stay wrong until its next commit.
pub fn set_scale(windows: &mut Windows, surface: &WlSurface, sx: f32, sy: f32) {
    if let Some(i) = index_of(windows, surface) {
        windows.scale_x[i] = sx.max(0.01);
        windows.scale_y[i] = sy.max(0.01);
    }
}

pub fn clear_scale(windows: &mut Windows, surface: &WlSurface) {
    if let Some(i) = index_of(windows, surface) {
        windows.scale_x[i] = 1.0;
        windows.scale_y[i] = 1.0;
    }
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
        windows.draw_x.remove(i);
        windows.draw_y.remove(i);
        windows.z.remove(i);
        windows.target_z.remove(i);
        windows.order.remove(i);
        windows.geo_x.remove(i);
        windows.geo_y.remove(i);
        windows.geo_w.remove(i);
        windows.geo_h.remove(i);
        windows.swizzle.remove(i);
        windows.scale_x.remove(i);
        windows.scale_y.remove(i);
        windows.filter.remove(i);
        windows.alpha.remove(i);
        windows.alpha_to.remove(i);
        windows.maximized.remove(i);
        windows.max_x.remove(i);
        windows.max_y.remove(i);
        windows.restore_x.remove(i);
        windows.restore_y.remove(i);
        windows.restore_w.remove(i);
        windows.restore_h.remove(i);
        windows.mip.remove(i);
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
        // Against the drawn origin, not the true one: the question is what the pointer was
        // over on screen, and what is on screen is the aligned rectangle the test above used.
        let local_x = cx - windows.draw_x[i];
        let local_y = cy - windows.draw_y[i];
        if state::input_region_contains(&windows.surface[i], local_x, local_y) {
            // Same keys as the draw pass, in the same order, or the thing you click is
            // not the thing on top: stack order first, then z within that stack entry,
            // then the later slot, which is what a stable sort draws last.
            let nearer = |b: usize| {
                windows.order[i] > windows.order[b]
                    || (windows.order[i] == windows.order[b]
                        && windows.z[i] <= windows.z[b])
            };
            if best.map_or(true, nearer) {
                best = Some(i);
            }
        }
    }
    // The true origin comes back, not the drawn one. This is what a drag anchors to, and a
    // drag that started from the rounded position would write the rounding into the position
    // it then moves.
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
// The true centre, not the drawn one: this pairs with set_window_center, which writes the
// true position, and reading the rounded one would feed the alignment back into the position
// every time a window is dropped.
pub fn window_center(windows: &Windows, surface: &WlSurface) -> Option<(f32, f32)> {
    index_of(windows, surface).map(|i| {
        (
            windows.canvas_x[i] + windows.geo_x[i] + windows.geo_w[i] * windows.scale_x[i] * 0.5,
            windows.canvas_y[i] + windows.geo_y[i] + windows.geo_h[i] * windows.scale_y[i] * 0.5,
        )
    })
}

// Place a window so its center sits at the given canvas position.
pub fn set_window_center(windows: &mut Windows, surface: &WlSurface, cx: f32, cy: f32) {
    if let Some(i) = index_of(windows, surface) {
        windows.canvas_x[i] = cx - windows.geo_w[i] * 0.5 - windows.geo_x[i];
        windows.canvas_y[i] = cy - windows.geo_h[i] * 0.5 - windows.geo_y[i];
        repin(windows, i);
    }
}

// Move a maximized window's pin to wherever the window now is.
//
// Every setter above ends here, so anything that moves a window moves the pin with it and
// hold_maximized has nothing to fight. Without this a maximized window could not be dragged at
// all: the drag moved it and the next frame put it back, once per frame, forever.
//
// Maximized here means shaped like the view was when you asked, not owning the screen the way
// it would on a desktop that had one. There is no reason a window that size cannot also be
// somewhere else, so a drag moves it and it stays maximized. Unmaximizing still gives back the
// rectangle it had before, size and position both.
fn repin(windows: &mut Windows, i: usize) {
    if !windows.maximized[i] {
        return;
    }
    windows.max_x[i] = windows.canvas_x[i] + windows.geo_x[i];
    windows.max_y[i] = windows.canvas_y[i] + windows.geo_y[i];
}

// Where a window's visible top-left sits on the canvas, geometry offset included: what a
// resize that drags a near edge has to hold still, and what a move has to offset from.
pub fn window_origin(windows: &Windows, surface: &WlSurface) -> Option<(f32, f32)> {
    index_of(windows, surface).map(|i| {
        (
            windows.canvas_x[i] + windows.geo_x[i],
            windows.canvas_y[i] + windows.geo_y[i],
        )
    })
}

// Put that visible top-left at a canvas position, which is not the same as setting the
// surface position: a client's geometry offset sits between the two.
pub fn set_window_origin(windows: &mut Windows, surface: &WlSurface, x: f32, y: f32) {
    if let Some(i) = index_of(windows, surface) {
        windows.canvas_x[i] = x - windows.geo_x[i];
        windows.canvas_y[i] = y - windows.geo_y[i];
        repin(windows, i);
    }
}

// Move a window to a canvas position.
pub fn set_window_pos(windows: &mut Windows, surface: &WlSurface, x: f32, y: f32) {
    if let Some(i) = index_of(windows, surface) {
        windows.canvas_x[i] = x;
        windows.canvas_y[i] = y;
        repin(windows, i);
    }
}

// Take a window's shape and remember the one it had, so unmaximizing can give it back.
//
// The rectangle passed in is the one being replaced, read before anything moves: the visible
// top-left and the geometry size, which are the two things maximizing overwrites.
pub fn maximize(
    windows: &mut Windows,
    surface: &WlSurface,
    // Where the visible top-left is to sit from now on.
    px: f32,
    py: f32,
    // And the rectangle being replaced, to be given back later.
    x: f32,
    y: f32,
    w: f32,
    h: f32,
) {
    let Some(i) = index_of(windows, surface) else {
        return;
    };
    windows.max_x[i] = px;
    windows.max_y[i] = py;
    // Already maximized: keep the rectangle from the first time. A client that asks twice
    // without being unmaximized in between would otherwise have the view's own shape recorded
    // as the one to go back to, and unmaximizing would put it nowhere.
    if windows.maximized[i] {
        return;
    }
    windows.maximized[i] = true;
    windows.restore_x[i] = x;
    windows.restore_y[i] = y;
    windows.restore_w[i] = w;
    windows.restore_h[i] = h;
}

// And give it back, once. None when this window was not maximized, so an unmaximize that
// answers nothing moves nothing.
pub fn unmaximize(windows: &mut Windows, surface: &WlSurface) -> Option<(f32, f32, f32, f32)> {
    let i = index_of(windows, surface)?;
    if !windows.maximized[i] {
        return None;
    }
    windows.maximized[i] = false;
    Some((windows.restore_x[i], windows.restore_y[i], windows.restore_w[i], windows.restore_h[i]))
}

pub fn is_maximized(windows: &Windows, surface: &WlSurface) -> bool {
    index_of(windows, surface).map(|i| windows.maximized[i]).unwrap_or(false)
}

// Whether a window's visible rectangle covers a canvas rectangle whole, which is the question
// "is this window filling the view right now" once the view is expressed in canvas units.
//
// A window's size and the view's are both changed by things the other does not know about: the
// window was sized in whole canvas units, the view rectangle is rarely whole, and the zoom has
// moved since. So a pixel of slack, or a window filling the screen as well as it can would be
// told that it is not.
pub fn covers(windows: &Windows, surface: &WlSurface, x: f32, y: f32, w: f32, h: f32) -> bool {
    let Some(i) = index_of(windows, surface) else {
        return false;
    };
    const SLACK: f32 = 1.0;
    let (wx, wy, ww, wh) = visible(windows, i);
    wx <= x + SLACK && wy <= y + SLACK && wx + ww >= x + w - SLACK && wy + wh >= y + h - SLACK
}

// Hold every maximized window's visible top-left where it was pinned.
//
// Run every frame, before children are placed and before positions are aligned, so a client
// that changes its geometry offset (dropping its shadow border because it now believes it is
// maximized, most of all) does not slide the window by the difference. Cheap: a maximized
// window is rare and the arithmetic is two subtractions.
pub fn hold_maximized(windows: &mut Windows) {
    for i in 0..windows.surface.len() {
        if !windows.maximized[i] {
            continue;
        }
        windows.canvas_x[i] = windows.max_x[i] - windows.geo_x[i];
        windows.canvas_y[i] = windows.max_y[i] - windows.geo_y[i];
    }
}

// Fade whatever is covering the window you are working in.
//
// Above it and overlapping it, both. Above, because a window behind the focused one hides nothing;
// overlapping, because one off to the side is not in the way however it is stacked. Everything else
// goes back to opaque.
//
// Eased rather than switched, since the answer changes whenever you click, drag or pan, and a hard
// cut on each of those reads as flicker rather than as an answer.
pub fn fade_covers(windows: &mut Windows, focused: Option<&WlSurface>, opacity: f32, dt: f32) {
    // How fast a window fades to where it is going, as a fraction of the gap per second. Fast
    // enough to feel immediate, slow enough that a click does not blink.
    const RATE: f32 = 14.0;

    let front = focused.and_then(|surface| index_of(windows, surface));
    for i in 0..windows.surface.len() {
        windows.alpha_to[i] = 1.0;
        let Some(f) = front else { continue };
        if i == f || windows.order[i] <= windows.order[f] {
            continue;
        }
        let (ax, ay, aw, ah) = visible(windows, i);
        let (bx, by, bw, bh) = visible(windows, f);
        let overlaps = ax < bx + bw && bx < ax + aw && ay < by + bh && by < ay + ah;
        if overlaps {
            windows.alpha_to[i] = opacity;
        }
    }
    let t = (RATE * dt).min(1.0);
    for i in 0..windows.surface.len() {
        windows.alpha[i] += (windows.alpha_to[i] - windows.alpha[i]) * t;
        if (windows.alpha_to[i] - windows.alpha[i]).abs() < 0.002 {
            windows.alpha[i] = windows.alpha_to[i];
        }
    }
}

// Land every window on whole pixels, every frame, the way the camera does. Same rule as
// camera::snapped_center from the other side of the same product: with the view already on a
// whole pixel, a canvas position whose product with the zoom is an integer puts the
// window's edges on pixel boundaries, so its texture is sampled 1:1 rather than blurred
// across two columns. At zoom 1 that is simply rounding to whole canvas units.
//
// Unconditional, rather than at the end of a drag: nothing then has to remember to round
// after moving a window, and a position that was never fractional cannot be caught
// fractional. Rounding to the pixel grid rather than to canvas units is what keeps a
// drag smooth when zoomed in, where one canvas unit is several pixels wide.
//
// Run this after everything that moves a window (drag, settle, child placement) and
// before the pixels are read for sampling decisions or drawn.
pub fn align_positions(windows: &mut Windows, zoom: f32) {
    // Away from 1:1 the window is filtered rather than sampled texel for texel, so there is
    // nothing for the grid to buy and the drawn position is simply the real one. Same test
    // the camera uses, so the two never disagree about which frames are aligned.
    if !camera::snaps_to_pixels(zoom) {
        for i in 0..windows.canvas_x.len() {
            windows.draw_x[i] = windows.canvas_x[i];
            windows.draw_y[i] = windows.canvas_y[i];
        }
        return;
    }
    for i in 0..windows.canvas_x.len() {
        windows.draw_x[i] = (windows.canvas_x[i] * zoom).round() / zoom;
        windows.draw_y[i] = (windows.canvas_y[i] * zoom).round() / zoom;
    }
}

// Copy every dmabuf window we are sampling in place into a texture of our own, so it can carry a
// mip chain.
//
// Only while minified, which is the whole idea: at 1:1 and above a chain buys nothing and holding
// the client's buffer costs nothing, so the copy would be pure loss. Below that the window is being
// shrunk and level 0 alone shimmers, and a copy is the only way to have the smaller levels at all.
// A dmabuf's level 0 is the client's buffer, and there is nowhere to put the rest.
//
// Once per window rather than once per frame: after this it is ours, so the next frame finds owns
// already true and does nothing. The client's next commit puts it back on the imported texture,
// freeing this one, and the copy happens again if the view is still out.
pub fn blit_minified(windows: &mut Windows, egl: &Egl, zoom: f32, below: f32) {
    if below <= 0.0 || zoom >= below {
        return;
    }
    for i in 0..windows.surface.len() {
        if windows.owns[i] || windows.tex_id[i] == 0 || windows.tex_w[i] <= 0 {
            continue;
        }
        let (w, h) = (windows.tex_w[i], windows.tex_h[i]);
        let dst = ray::load_texture_rgba(std::ptr::null(), w, h);
        if dst == 0 {
            continue;
        }
        if !egl.copy_into(windows.tex_id[i], dst, w, h) {
            // The driver will not render from this image. Nothing is lost: the imported texture is
            // still there and still draws, it simply cannot have a chain.
            ray::unload_texture(dst);
            continue;
        }
        // The old texture belongs to the dmabuf cache, so it is dropped rather than freed.
        windows.tex_id[i] = dst;
        windows.owns[i] = true;
        // An imported image samples as correct RGBA and so does a copy of it.
        windows.swizzle[i] = 0.0;
        // A new texture, with nothing set on it and no chain yet.
        windows.mip[i] = MIP_NONE;
        windows.filter[i] = FILTER_UNSET;
    }
}

// Decide how each window is sampled. Runs every frame, after everything that could move
// a window and before anything is drawn.
//
// The policy:
//
//   nearest    At 1:1 and above, on a window that is not lifted. Exact texels, no
//              blending, which is the sharpest a client's own rendering can look and the
//              right answer for magnifying: showing bigger pixels beats showing a blur
//              of pixels that were never rendered. Note that above 1:1 the grid is
//              deliberately let go (camera::snaps_to_pixels), so a magnified window is
//              sampled texel for texel from a position that is no longer aligned.
//   bilinear   Zoomed out, and on a lifted window, where the scale is neither 1:1 nor
//              constant across the quad.
//
// A lift is excluded because it magnifies through perspective while the window is in
// motion, and a smooth blur reads better there than crawling texel blocks.
//
// MIPS_WHEN_MINIFIED turns on the better answer for zooming out: a mip chain per shm
// texture, sampled trilinear with anisotropy. It measurably reduces the shimmer on
// minified text, but it only ever applies to half the windows, so it is off while the
// zoomed-out look is still being judged. Only textures we own can have one. A dmabuf
// texture's level 0 is an EGLImage the client owns, and building the smaller levels
// around it is not something a driver has to support: Mesa reports no error and creates
// nothing, so glGetError cannot tell us it failed. Switching that texture to a mipmap
// min filter then leaves it incomplete, an incomplete texture samples as transparent
// black, and our shader discards on zero alpha, so the window silently vanishes the
// moment you zoom out. That is exactly what Chromium did.
pub fn prepare_textures(windows: &mut Windows, zoom: f32, anisotropy: f32, mips: bool) {
    // Below this a window is being shrunk; at or above it, it is being magnified.
    const NATIVE_ZOOM: f32 = 0.999;
    // A lift this small is a window that has finished settling.
    const FLAT_Z: f32 = 0.5;
    // And a stretch this small is not one.
    const SCALE_EPS: f32 = 0.001;

    let magnified = zoom >= NATIVE_ZOOM;

    for i in 0..windows.surface.len() {
        if !drawable(windows, i) {
            continue;
        }
        let flat = windows.z[i].abs() < FLAT_Z;
        // A window being stretched by a resize drag is not at 1:1 whatever the zoom is,
        // so it wants the smooth filter, not exact texels of the wrong size.
        let unscaled = (windows.scale_x[i] - 1.0).abs() < SCALE_EPS
            && (windows.scale_y[i] - 1.0).abs() < SCALE_EPS;

        // A chain, if this window is being shrunk and does not have one yet.
        if mips && !magnified && windows.mip[i] == MIP_NONE {
            windows.mip[i] = if !windows.owns[i] {
                MIP_REFUSED
            } else if egl::build_mips(windows.tex_id[i], anisotropy) {
                // build_mips leaves the texture on trilinear.
                windows.filter[i] = FILTER_TRILINEAR;
                MIP_READY
            } else {
                MIP_REFUSED
            };
        }

        let want = if magnified && flat && unscaled {
            FILTER_NEAREST
        } else if mips && !magnified && windows.mip[i] == MIP_READY {
            FILTER_TRILINEAR
        } else {
            FILTER_LINEAR
        };
        if want == windows.filter[i] {
            continue;
        }
        match want {
            FILTER_NEAREST => egl::set_filter_nearest(windows.tex_id[i]),
            FILTER_TRILINEAR => egl::set_filter_trilinear(windows.tex_id[i], anisotropy),
            _ => egl::set_filter_linear(windows.tex_id[i]),
        }
        windows.filter[i] = want;
    }
}

// A label under every window, in the canvas rather than in a corner of the screen:
// what it is made of, how it is being sampled and at what scale. Reading it off the
// same columns the draw pass reads means it cannot drift from what is actually
// happening, which is the whole point of having it.
//
// Drawn in 2D at the projected canvas point, so it pans and scales with the view and
// rides along when a window is lifted. The size follows the zoom with a floor, since a
// label that scales all the way down is unreadable exactly when you are zoomed out
// looking for why something shimmers.
pub fn draw_debug_labels(windows: &Windows, cam3d: ray::Camera3D, zoom: f32, anisotropy: f32) {
    const BASE_SIZE: f32 = 13.0;
    const MIN_SIZE: i32 = 10;
    const PAD: i32 = 3;
    let fg = ray::Color { r: 240, g: 230, b: 120, a: 255 };
    let bg = ray::Color { r: 0, g: 0, b: 0, a: 170 };

    let size = ((BASE_SIZE * zoom) as i32).max(MIN_SIZE);
    for i in 0..windows.surface.len() {
        if !drawable(windows, i) {
            continue;
        }
        let (x, y, w, h) = visible(windows, i);
        // Bottom-left of the window, at its own lifted z so the label travels with it.
        let anchor = ray::world_to_screen(
            Vector3 { x, y: y + h, z: windows.z[i] },
            cam3d,
        );

        let kind = if windows.popup[i] {
            "popup"
        } else if windows.sub[i] {
            "sub"
        } else {
            "window"
        };
        let source = if windows.owns[i] { "shm" } else { "dmabuf" };
        // What the sampler is actually set to, read from the column prepare_textures
        // wrote, with the reason when it is not the best available.
        let sampling = match windows.filter[i] {
            FILTER_NEAREST => "nearest".to_string(),
            FILTER_TRILINEAR => format!("trilinear+{anisotropy:.0}x aniso"),
            _ if windows.mip[i] == MIP_REFUSED && !windows.owns[i] => {
                "bilinear (no mips on dmabuf)".to_string()
            }
            _ if windows.mip[i] == MIP_REFUSED => "bilinear (driver refused mips)".to_string(),
            _ => "bilinear".to_string(),
        };
        // Screen pixels per texel: the zoom, times what perspective adds for a lifted
        // window. Measured off the quad itself rather than assumed, so a lifted window
        // reports the scale it is really being drawn at.
        let right = ray::world_to_screen(
            Vector3 { x: x + w, y: y + h, z: windows.z[i] },
            cam3d,
        );
        let scale = if w > 0.0 { (right.x - anchor.x) / w } else { zoom };

        let lines = [
            format!("{kind} {source} tex{} {}x{}", windows.tex_id[i], windows.tex_w[i], windows.tex_h[i]),
            format!("{sampling}  scale {scale:.2}x"),
            format!(
                "canvas {:.1},{:.1}  geo {:.0},{:.0} {:.0}x{:.0}  z {:.2}",
                windows.canvas_x[i], windows.canvas_y[i],
                windows.geo_x[i], windows.geo_y[i], windows.geo_w[i], windows.geo_h[i],
                windows.z[i],
            ),
        ];

        let widest = lines.iter().map(|l| ray::measure_text(l, size)).max().unwrap_or(0);
        let line_h = size + PAD;
        let top = anchor.y as i32 + PAD;
        ray::draw_rectangle(
            anchor.x as i32 - PAD,
            top - PAD,
            widest + PAD * 2,
            line_h * lines.len() as i32 + PAD,
            bg,
        );
        for (n, line) in lines.iter().enumerate() {
            ray::draw_text(line, anchor.x as i32, top + line_h * n as i32, size, fg);
        }
    }
}


// Frame timing, top left. Three numbers because one is not enough to see lag: the rate,
// this frame, and the worst frame in the last second or so. An average hides exactly the
// stutter you are looking for, so the worst is what to watch while dragging something.
//
// Coloured by the worst rather than the average, and against the frame budget: green
// while every frame fits in a 60Hz refresh, amber when some frame missed it, red when
// something missed it badly.
pub fn draw_frame_stats(fps: f32, ms: f32, worst_ms: f32) {
    const PAD: i32 = 6;
    const TEXT: i32 = 14;
    let bg = ray::Color { r: 12, g: 12, b: 16, a: 190 };
    let colour = if worst_ms <= 17.0 {
        ray::Color { r: 130, g: 230, b: 140, a: 255 }
    } else if worst_ms <= 25.0 {
        ray::Color { r: 240, g: 210, b: 110, a: 255 }
    } else {
        ray::Color { r: 250, g: 120, b: 90, a: 255 }
    };
    // The latch margin is here because it is the number you tune against the other
    // three: it is how far before the vblank the frame was composed.
    let line = format!("{fps:.0} fps   {ms:.1} ms   worst {worst_ms:.1} ms");
    let w = ray::measure_text(&line, TEXT);
    ray::draw_rectangle(PAD, PAD, w + PAD * 2, TEXT + PAD * 2, bg);
    ray::draw_text(&line, PAD * 2, PAD * 2, TEXT, colour);
}

// The trackpad, drawn in the bottom right corner: the surface at its real aspect ratio,
// the button regions marked, a dot per finger, and what the gesture code currently
// thinks is happening. Screen space, not canvas: it is an instrument, not content.
//
// Everything here comes from touch::view, so it shows what the gesture code decided
// rather than a second interpretation of the same events.
pub fn draw_pad_debug(pad: &touch::PadView, screen_w: i32, screen_h: i32) {
    const WIDTH: i32 = 260;
    const MARGIN: i32 = 16;
    const TEXT: i32 = 12;
    const LINE: i32 = 15;

    let bg = ray::Color { r: 12, g: 12, b: 16, a: 210 };
    let frame = ray::Color { r: 90, g: 95, b: 110, a: 255 };
    let region = ray::Color { r: 60, g: 65, b: 80, a: 255 };
    let live = ray::Color { r: 240, g: 230, b: 120, a: 255 };
    let hot = ray::Color { r: 250, g: 120, b: 90, a: 255 };
    let dim = ray::Color { r: 150, g: 155, b: 170, a: 255 };

    // The pad, at its own aspect ratio so the regions look like they feel.
    let pad_w = WIDTH;
    let pad_h = if pad.aspect > 0.1 { (WIDTH as f32 / pad.aspect) as i32 } else { WIDTH / 2 };
    let lines = 5;
    let panel_h = pad_h + LINE * lines + MARGIN;
    let x0 = screen_w - WIDTH - MARGIN;
    let y0 = screen_h - panel_h - MARGIN;

    ray::draw_rectangle(x0 - 8, y0 - 8, WIDTH + 16, panel_h + 16, bg);

    let px = x0;
    let py = y0 + LINE * lines;
    ray::draw_rectangle_lines(px, py, pad_w, pad_h, frame);

    // The resting zone: below this line a still finger is treated as parked.
    if pad.rest_zone > 0.0 {
        let rest_y = py + (pad_h as f32 * (1.0 - pad.rest_zone)) as i32;
        ray::draw_line(px, rest_y, px + pad_w, rest_y, ray::Color { r: 45, g: 50, b: 62, a: 255 });
    }

    // Button regions: the strip along the bottom, split in two.
    if pad.software_buttons {
        let strip_y = py + (pad_h as f32 * (1.0 - pad.strip)) as i32;
        let split_x = px + (pad_w as f32 * pad.split) as i32;
        ray::draw_line(px, strip_y, px + pad_w, strip_y, region);
        ray::draw_line(split_x, strip_y, split_x, py + pad_h, region);
        let label_y = py + pad_h - TEXT - 4;
        ray::draw_text("L", px + 6, label_y, TEXT, if pad.left { live } else { region });
        ray::draw_text("R", split_x + 6, label_y, TEXT, if pad.right { live } else { region });
    }

    // Fingers. Parked ones are drawn dim and small, since nothing reads them; of the
    // rest, the lowest is the one that would decide a click, so mark that one.
    let mut lowest: Option<usize> = None;
    for i in 0..pad.count {
        if pad.resting[i] || pad.faint[i] {
            continue;
        }
        if lowest.map_or(true, |b| pad.fingers[i].1 > pad.fingers[b].1) {
            lowest = Some(i);
        }
    }
    for i in 0..pad.count {
        let (fx, fy) = pad.fingers[i];
        let cx = px + (fx * pad_w as f32) as i32;
        let cy = py + (fy * pad_h as f32) as i32;
        let deciding = pad.software_buttons && lowest == Some(i);
        let colour = if pad.faint[i] {
            // Below the size threshold: drawn, because you need to see that it is there
            // and being ignored, but in the frame colour to say nothing reads it.
            frame
        } else if pad.resting[i] {
            dim
        } else if deciding {
            hot
        } else {
            live
        };
        // The contact ellipse, at the scale the pad is drawn: how much of the finger is
        // actually touching. This hardware reports no per-finger pressure, so the
        // footprint is the only force-like signal there is, and a resting thumb is
        // visibly fatter than a pointing fingertip.
        if pad.has_size {
            let (major, minor) = pad.size[i];
            // major and minor are diameters, so halve them. Floored at a couple of
            // pixels: a fingertip's contact is only a few millimetres, which is smaller
            // than the dot at this panel size, and an invisible footprint teaches
            // nothing.
            let ry = (major * 0.5 * pad_h as f32).max(2.5);
            let rx = (minor * 0.5 * pad_h as f32).max(2.0);
            ray::draw_ellipse_lines(cx, cy, rx, ry, colour);
        }
        // And the tracked point itself, which is what every decision reads.
        ray::draw_circle(cx, cy, 1.5, colour);
    }

    // What it thinks is going on.
    let armed = |on: bool| if on { "armed" } else { "idle" };
    let text = [
        format!(
            "trackpad  {} finger{}  {} counted  max {}",
            pad.count,
            if pad.count == 1 { "" } else { "s" },
            pad.active,
            pad.contact_max
        ),
        format!(
            "gesture {}  pan {}  zoom {}",
            pad.mode,
            armed(pad.pan_armed),
            armed(pad.zoom_armed)
        ),
        format!(
            "pan {:+.1},{:+.1}  zoom {:.3}  cursor {}",
            pad.pan.0, pad.pan.1, pad.zoom, armed(pad.ptr_armed)
        ),
        format!(
            "size{}{}{}{}  load {}/{}",
            if pad.count > 0 { " " } else { " -" },
            if pad.count > 0 { format!("{}", pad.major[0]) } else { String::new() },
            if pad.count > 1 { format!(" {}", pad.major[1]) } else { String::new() },
            if pad.count > 2 { format!(" {}", pad.major[2]) } else { String::new() },
            pad.load,
            pad.load_max
        ),
        format!(
            "buttons {}{}{}",
            if pad.software_buttons { "regions" } else { "physical" },
            if pad.left { "  LEFT" } else { "" },
            if pad.right { "  RIGHT" } else { "" }
        ),
    ];
    for (n, line) in text.iter().enumerate() {
        let colour = if n == 4 && (pad.left || pad.right) { live } else { dim };
        ray::draw_text(line, x0, y0 + LINE * n as i32, TEXT, colour);
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
pub fn set_place_origin(windows: &mut Windows, cx: f32, cy: f32, spawn: Spawn) {
    windows.place_at = (cx, cy);
    windows.spawn = spawn;
}

// Which of the four directions a candidate lies in, from the centre it was measured against.
//
// The dominant axis, so a candidate up and slightly right counts as up. A diagonal, where neither
// leads, belongs to no direction at all and is only taken once the preferred ones are exhausted.
fn direction_of(dx: i32, dy: i32) -> u8 {
    if dx.abs() > dy.abs() {
        if dx > 0 {
            DIR_RIGHT
        } else {
            DIR_LEFT
        }
    } else if dy.abs() > dx.abs() {
        // Screen coordinates: y grows downward, so up is negative.
        if dy < 0 {
            DIR_UP
        } else {
            DIR_DOWN
        }
    } else {
        DIR_NONE
    }
}

// How much a direction is preferred, lower being sooner. Anything unlisted comes after everything
// listed, which is what makes a short order mean "these first, then whatever is nearest".
fn direction_rank(order: &[u8; 4], dir: u8) -> u8 {
    for (rank, &want) in order.iter().enumerate() {
        if want == DIR_NONE {
            break;
        }
        if want == dir {
            return rank as u8;
        }
    }
    u8::MAX
}

// Whether a rectangle would land clear of every window already on the canvas, with a gap around it.
//
// Children are ignored: a popup belongs to its parent and is placed off it every frame, so treating
// a menu as an obstacle would push new windows away from a shape that is about to disappear.
fn spot_free(windows: &Windows, x: f32, y: f32, w: f32, h: f32, gap: f32) -> bool {
    for i in 0..windows.surface.len() {
        if child(windows, i) || !drawable(windows, i) {
            continue;
        }
        let (bx, by, bw, bh) = visible(windows, i);
        if x - gap < bx + bw && bx < x + w + gap && y - gap < by + bh && by < y + h + gap {
            return false;
        }
    }
    true
}

// The nearest window in a direction from a point, or None when there is nothing that way.
//
// A cone rather than a half plane: a window at right angles to the swipe is not "that way", however
// close it is, and treating it as a candidate would make the gesture feel like it was guessing.
// Within the cone the score is how far along the direction it lies plus a penalty for how far off
// the line, so a window straight ahead beats a nearer one off to the side.
pub fn nearest_in_direction(
    windows: &Windows,
    from: (f32, f32),
    dir: (f32, f32),
) -> Option<WlSurface> {
    // How far off the line a candidate may be, as a fraction of how far along it is. 1.0 is a
    // forty-five degree cone either side, which covers a hand that swipes diagonally by accident
    // without reaching windows nobody was pointing at.
    const SPREAD: f32 = 1.0;
    // And how much being off the line costs against being far away, which is what makes a window
    // straight ahead win over a closer one to the side.
    const OFF_AXIS: f32 = 2.0;

    let mut best: Option<(f32, usize)> = None;
    for i in 0..windows.surface.len() {
        if child(windows, i) || !drawable(windows, i) {
            continue;
        }
        let (x, y, w, h) = visible(windows, i);
        let (vx, vy) = (x + w * 0.5 - from.0, y + h * 0.5 - from.1);
        let along = vx * dir.0 + vy * dir.1;
        // Behind you, or where you already are.
        if along <= 1.0 {
            continue;
        }
        let across = (vx * dir.1 - vy * dir.0).abs();
        if across > along * SPREAD {
            continue;
        }
        let score = along + across * OFF_AXIS;
        if best.map_or(true, |(b, _)| score < b) {
            best = Some((score, i));
        }
    }
    best.map(|(_, i)| windows.surface[i].clone())
}

// Where to put a new window: the nearest place to the middle of the view where it lands on nothing.
//
// A cascade is what a desktop does because a desktop has one screenful and no choice. A canvas has
// room, and a window arriving on top of what you are reading is a decision nobody made. Nearest to
// the middle rather than anywhere free, because a window that opens where you are not looking is
// its own kind of lost.
//
// Rings outward from the centre, taking the closest free candidate on the first ring that has one.
// Rings rather than a scan, so the answer is near by construction rather than by sorting everything.
// The step is half a window, fine enough not to miss a gap that would have fitted and coarse enough
// to cross a screenful in a handful of rings.
fn free_spot(windows: &Windows, w: f32, h: f32) -> (f32, f32) {
    const RINGS: i32 = 16;
    let (cx, cy) = windows.place_at;
    let gap = windows.spawn.gap;
    let order = windows.spawn.order;
    let step = (w.max(h) * 0.5).max(64.0);

    for ring in 0..=RINGS {
        // Ranked first by which direction it lies in, then by how far it is. With no preferences
        // every candidate ranks the same and this is purely nearest, which is where it started.
        let mut best: Option<(u8, f32, f32, f32)> = None;
        for dy in -ring..=ring {
            for dx in -ring..=ring {
                // The perimeter of this ring only: the inside was searched by the rings before it.
                if ring > 0 && dx.abs() != ring && dy.abs() != ring {
                    continue;
                }
                let (ox, oy) = (cx + dx as f32 * step, cy + dy as f32 * step);
                let (x, y) = ((ox - w * 0.5).round(), (oy - h * 0.5).round());
                if !spot_free(windows, x, y, w, h, gap) {
                    continue;
                }
                let rank = direction_rank(&order, direction_of(dx, dy));
                let d2 = (ox - cx) * (ox - cx) + (oy - cy) * (oy - cy);
                if best.map_or(true, |(br, bd, _, _)| (rank, d2) < (br, bd)) {
                    best = Some((rank, d2, x, y));
                }
            }
        }
        if let Some((_, _, x, y)) = best {
            return (x, y);
        }
    }
    // A canvas crowded for sixteen rings in every direction. Land in the middle and overlap, which
    // is at least where you are looking.
    ((cx - w * 0.5).round(), (cy - h * 0.5).round())
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
    // a popup's children are placed off the popup we just moved. The tree comes back in the
    // client's stacking order with the root at its own slot.
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
            // In front of the root, every one of them, and ordered among themselves by the
            // slot the client gave them read backwards: the earliest slot is nearest the
            // camera.
            //
            // Backwards because that is what the only client we have with more than one
            // subsurface actually means. Chromium builds its overlays by placing each new one
            // below the last, so the list runs front to back, the exact reverse of what the
            // protocol says it is. Measured twice, with the tree dumped from a live session:
            //
            //     tree 3 surfaces, root at slot 2
            //       slot 0  350x94   the tooltip      belongs in front
            //       slot 1  334x458  the QR bubble    belongs in the middle
            //       slot 2  968x808  the root         belongs at the back
            //
            // Read literally that says the tooltip is under the bubble and both are under an
            // opaque browser window, which would make all of it invisible, and no client asks
            // for that. Read backwards it is exactly right, and it also explains the Restore
            // pages bubble that started this: one subsurface, below the root, meant to be on
            // top of it.
            //
            // So the tree's stacking is taken as front to back rather than back to front, and
            // the root is always the back. What this gives up is a client that uses the order
            // as written: a drop shadow placed genuinely behind the window would come out in
            // front of it, over the content and swallowing clicks. Nothing we run does that,
            // every weston client here has a single surface and no subsurfaces at all, but
            // that is the trade and it is the first thing to suspect if a client ever comes
            // back looking veiled or stacked inside out.
            let steps = -((tree.len() - slot) as f32);
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
    // Snapped once it is within a hair of the target, because an asymptote never arrives.
    // A window that had been lifted once used to keep a sliver of negative z forever,
    // which is a sliver of "nearer the camera" that no amount of raising could undo.
    const SETTLED: f32 = 0.01;
    let t = (LIFT_RATE * dt).min(1.0);
    for i in 0..windows.z.len() {
        let target = windows.target_z[i];
        if (target - windows.z[i]).abs() < SETTLED {
            windows.z[i] = target;
            continue;
        }
        windows.z[i] += (target - windows.z[i]) * t;
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
            // New texture, so no chain and nothing set on it yet.
            windows.mip[i] = MIP_NONE;
            windows.filter[i] = FILTER_UNSET;
        }
        None => {
            // Centre new windows on the view, stepped so successive ones do not
            // hide each other. Popups are not placed at all: sync_popups puts them
            // under their parent before the first draw.
            const CASCADE_STEP: f32 = 48.0;
            const CASCADE_WRAP: u32 = 6;
            let (cx, cy) = if popup || sub {
                (0.0, 0.0)
            } else if windows.spawn.clear {
                // The buffer's size, since geometry is not known until the client's first commit.
                // That reads slightly larger than the window is, by whatever it pads around itself,
                // which errs toward leaving room rather than toward crowding.
                free_spot(windows, w as f32, h as f32)
            } else {
                let step = (windows.cascade % CASCADE_WRAP) as f32 * CASCADE_STEP;
                windows.cascade += 1;
                // Rounded, because centring a window with an odd dimension lands it on
                // a half unit, and a window off the pixel grid can never be drawn at
                // 1:1 with exact texels (see prepare_textures).
                (
                    (windows.place_at.0 - w as f32 * 0.5 + step).round(),
                    (windows.place_at.1 - h as f32 * 0.5 + step).round(),
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
            // Aligned before the first draw, but seeded here so a window is never drawn
            // from an uninitialised slot if anything reads it earlier.
            windows.draw_x.push(cx);
            windows.draw_y.push(cy);
            windows.z.push(0.0);
            windows.target_z.push(0.0);
            windows.order.push(order);
            windows.geo_x.push(0.0);
            windows.geo_y.push(0.0);
            windows.geo_w.push(w as f32);
            windows.geo_h.push(h as f32);
            windows.swizzle.push(swizzle);
            windows.scale_x.push(1.0);
            windows.scale_y.push(1.0);
            windows.filter.push(FILTER_UNSET);
            windows.alpha.push(1.0);
            windows.alpha_to.push(1.0);
            windows.maximized.push(false);
            windows.max_x.push(0.0);
            windows.max_y.push(0.0);
            windows.restore_x.push(0.0);
            windows.restore_y.push(0.0);
            windows.restore_w.push(0.0);
            windows.restore_h.push(0.0);
            windows.mip.push(MIP_NONE);
            windows.owns.push(owns);
            windows.popup.push(popup);
            windows.sub.push(sub);
            windows.placed.push(false);
            if !popup && !sub {
                windows.mapped.push(surface.clone());
            }
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

// Drop the cached import of a buffer the client has destroyed, and unhook any window that is
// still displaying it.
//
// Both halves, or neither. The cache owns the texture and deletes it here, but a window that
// was drawn from it keeps the name in tex_id, and a deleted name is not a texture: the window
// would sample nothing and draw as a hole for as long as its client went without committing
// again. Clearing the entry makes it undrawable instead, which is the honest description of a
// window whose pixels have been taken away, and its next commit puts it back.
//
// Windows that own their texture are left alone. An shm upload, or a dmabuf copied out under
// DmabufMode::Blit, is ours and does not go away with the client's buffer.
pub fn evict_dmabuf(
    windows: &mut Windows,
    cache: &mut DmabufCache,
    egl: &Egl,
    key: &ObjectId,
) {
    let Some(tex) = cache.evict(egl, key) else { return };
    for i in 0..windows.tex_id.len() {
        if !windows.owns[i] && windows.tex_id[i] == tex {
            windows.tex_id[i] = 0;
            windows.tex_w[i] = 0;
            windows.tex_h[i] = 0;
        }
    }
}

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

    // Returns the texture it destroyed, because a window may still be pointing at it and the
    // caller has to know which one. See evict_dmabuf.
    fn evict(&mut self, egl: &Egl, key: &ObjectId) -> Option<u32> {
        let i = self.index_of(key)?;
        let tex = self.tex[i];
        egl.destroy(self.image[i], tex);
        self.key.swap_remove(i);
        self.image.swap_remove(i);
        self.tex.swap_remove(i);
        self.w.swap_remove(i);
        self.h.swap_remove(i);
        Some(tex)
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
    mode: DmabufMode,
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
        let Some((tex, w, h)) = cache.get_or_import(egl, key, info) else {
            eprintln!(
                "om_wm: dmabuf import failed {}x{} fourcc={:#x} mod={:#x}",
                info.width, info.height, info.fourcc, info.modifier
            );
            // This buffer is no use to us, but the window still draws from the one before it,
            // which therefore has to stay held.
            return Keep::Skip;
        };
        match mode {
            DmabufMode::Blit if copy_dmabuf(windows, egl, surface, tex, w, h, popup, sub) => {
                // The pixels are ours now, so the client can have its buffer straight back.
                Keep::Release
            }
            // Either we were told to sample in place, or the copy failed and sampling in
            // place is the only way left to draw it. Both mean the client's buffer is what
            // we read all frame, so both have to hold it.
            DmabufMode::Blit | DmabufMode::Hold => {
                store_entry(windows, surface, tex, w, h, 0.0, false, popup, sub);
                Keep::Hold
            }
        }
    });
    set_geometry(windows, surface, state::geometry_of(surface));
}

// Copy an imported client buffer into a texture we own and store that instead. Reuses the
// texture in place while the size holds, the way the shm path does, so a client redrawing at
// a steady size allocates nothing per frame.
//
// False when the driver will not render from the imported image, which leaves the caller to
// sample it in place.
fn copy_dmabuf(
    windows: &mut Windows,
    egl: &Egl,
    surface: &WlSurface,
    src: u32,
    w: i32,
    h: i32,
    popup: bool,
    sub: bool,
) -> bool {
    if let Some(i) = index_of(windows, surface) {
        if windows.owns[i] && windows.tex_w[i] == w && windows.tex_h[i] == h {
            if !egl.copy_into(src, windows.tex_id[i], w, h) {
                return false;
            }
            windows.swizzle[i] = 0.0;
            // Same texture, new pixels: any chain it carries describes the old ones. A
            // refusal stays, being a property of the texture rather than of the content.
            if windows.mip[i] != MIP_REFUSED {
                windows.mip[i] = MIP_NONE;
            }
            return true;
        }
    }

    let dst = ray::load_texture_rgba(std::ptr::null(), w, h);
    if dst == 0 {
        return false;
    }
    if !egl.copy_into(src, dst, w, h) {
        ray::unload_texture(dst);
        return false;
    }
    egl::set_filter_linear(dst);
    // Imported images sample as correct RGBA, and a copy of them is still correct RGBA, so
    // this owns its texture like an shm window without wanting shm's swizzle.
    store_entry(windows, surface, dst, w, h, 0.0, true, popup, sub);
    true
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
            // Same texture, new pixels: whatever chain it had describes the old ones.
            // Keep a refusal, since that is a property of the texture, not the content.
            if windows.mip[i] != MIP_REFUSED {
                windows.mip[i] = MIP_NONE;
            }
            return;
        }
    }

    let id = ray::load_texture_rgba(data, w, h);
    // raylib's own default here is nearest on both filters. Say what we want instead,
    // so an shm window and a dmabuf window are sampled the same way.
    egl::set_filter_linear(id);
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
    windows.draw_x.clear();
    windows.draw_y.clear();
    windows.mapped.clear();
    windows.z.clear();
    windows.target_z.clear();
    windows.order.clear();
    windows.geo_x.clear();
    windows.geo_y.clear();
    windows.geo_w.clear();
    windows.geo_h.clear();
    windows.swizzle.clear();
    windows.scale_x.clear();
    windows.scale_y.clear();
    windows.filter.clear();
    windows.alpha.clear();
    windows.alpha_to.clear();
    windows.maximized.clear();
    windows.max_x.clear();
    windows.max_y.clear();
    windows.restore_x.clear();
    windows.restore_y.clear();
    windows.restore_w.clear();
    windows.restore_h.clear();
    windows.mip.clear();
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
    fade_loc: i32,
    // Whether to draw what the client padded around its window, which is where its shadow and
    // its rounded corners live. See the quad below.
    shadows: bool,
) {
    // Painter's order: stack order first, then z within a stack entry. Depth test is off,
    // so what is drawn last wins.
    //
    // Order is the primary key because it is what identifies a window and everything
    // hanging off it: a popup and every subsurface inherit their root's order, and z only
    // says where they sit relative to that root, a twentieth of a unit apart. Sorting by z
    // first meant any window that had ever been lifted floated above windows that had not,
    // permanently, since the lift animation approaches zero without reaching it; raising
    // another window could not help, because z decided everything and raising only changes
    // order. Which also made a freshly mapped window appear underneath.
    let mut idx: Vec<usize> = (0..windows.surface.len())
        .filter(|&i| drawable(windows, i))
        .collect();
    idx.sort_by(|&a, &b| {
        windows.order[a].cmp(&windows.order[b]).then(
            windows.z[b]
                .partial_cmp(&windows.z[a])
                .unwrap_or(std::cmp::Ordering::Equal),
        )
    });

    ray::begin_mode_3d(cam3d);
    ray::disable_backface_culling();
    ray::disable_depth_test();
    for i in idx {
        let (x, y, w, h) = visible(windows, i);
        let z = windows.z[i];
        let tw = windows.tex_w[i] as f32;
        let th = windows.tex_h[i] as f32;
        // A client draws more than its window. GTK and every toolkit like it commit a surface
        // wider than the geometry they declare, and the difference is a shadow, and often a
        // rounded corner cut out of it. We cannot ask them to stop: server-side decorations
        // are a thing those toolkits do not implement, so the padding is in the buffer whether
        // we want it or not.
        //
        // Cropping to the geometry is the honest reading of the protocol and it looks wrong:
        // the shadow is sliced off square, and the corners the client rounded are filled back
        // in with whatever the window's edge pixels were. So draw the whole surface instead.
        //
        // Only the drawing changes. Everything that reasons about where a window is, hit
        // tests, placement, drags, maximizing, still works in the geometry rectangle that
        // visible() gives, because that is what the window is. The padding is scenery: it can
        // hang over a neighbour and cannot be clicked.
        //
        // The stretch of a resize drag is anchored at the geometry's top-left, so the padding
        // is scaled about that same point rather than about itself.
        let (x, y, w, h, u0, v0, u1, v1) = if shadows {
            let (sx, sy) = (windows.scale_x[i], windows.scale_y[i]);
            (
                x - windows.geo_x[i] * sx,
                y - windows.geo_y[i] * sy,
                tw * sx,
                th * sy,
                0.0,
                0.0,
                1.0,
                1.0,
            )
        } else {
            (
                x,
                y,
                w,
                h,
                windows.geo_x[i] / tw,
                windows.geo_y[i] / th,
                (windows.geo_x[i] + w) / tw,
                (windows.geo_y[i] + h) / th,
            )
        };
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
        ray::set_shader_float(shader, fade_loc, windows.alpha[i]);
        ray::draw_textured_quad(windows.tex_id[i], corners);
        ray::end_shader_mode();
    }
    ray::enable_depth_test();
    ray::enable_backface_culling();
    ray::end_mode_3d();
}
