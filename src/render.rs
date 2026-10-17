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
use crate::egl::{self, Egl, EglImage};
use crate::ray::{self, Vector3};
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
    // A handle for code outside the protocol zone, scripts included, to name a window by.
    // Never reused, so a handle to a window that has gone simply stops resolving, which is
    // the job a generation counter does for a reused slot.
    pub id: Vec<u64>,
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
    // The quad's stretch, and 1.0 the rest of the time.
    //
    // A resize on Wayland is a request: we ask, the client renders, and its pixels arrive a
    // round trip later. Unstretched, the window is only ever the size the client has
    // committed. Stretched, the quad is scaled to the size being asked for (shown_w, shown_h)
    // while the client catches up, so the corner tracks your hand at the cost of showing
    // content at a size it was not drawn at. Whether a window is stretched is the scripts'
    // (yk.window_look); the scale is worked out here every frame from what the client has
    // committed, so its commits change the pixels without moving the corner.
    pub scale_x: Vec<f32>,
    pub scale_y: Vec<f32>,
    pub stretch: Vec<bool>,
    pub shown_w: Vec<f32>,
    pub shown_h: Vec<f32>,
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
    // Whether to draw what the client padded around its window, which is where its shadow and
    // its rounded corners live. See draw_windows.
    pub shadow: Vec<bool>,
    // Which shader slot draws this window (0 is the built-in one), and four numbers of the
    // scripts' own for it, handed to the shader as windowParams.
    pub shader: Vec<u8>,
    pub params: Vec<[f32; 4]>,
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
    // Where new windows start: the canvas point at the middle of the view, which the main
    // loop keeps current. The scripts move them from there when they map.
    place_at: (f32, f32),
    // Next stack order to assign (monotonic; higher = more recently raised).
    next_order: u32,
    // Next handle to hand out. Starts at 1 so 0 is never a window.
    next_id: u64,
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
        id: Vec::new(),
        tex_id: Vec::new(),
        tex_w: Vec::new(),
        tex_h: Vec::new(),
        canvas_x: Vec::new(),
        canvas_y: Vec::new(),
        draw_x: Vec::new(),
        draw_y: Vec::new(),
        z: Vec::new(),
        order: Vec::new(),
        geo_x: Vec::new(),
        geo_y: Vec::new(),
        geo_w: Vec::new(),
        geo_h: Vec::new(),
        swizzle: Vec::new(),
        scale_x: Vec::new(),
        scale_y: Vec::new(),
        stretch: Vec::new(),
        shown_w: Vec::new(),
        shown_h: Vec::new(),
        filter: Vec::new(),
        alpha: Vec::new(),
        shadow: Vec::new(),
        shader: Vec::new(),
        params: Vec::new(),
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
        place_at: (0.0, 0.0),
        next_order: 0,
        next_id: 1,
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

// Everything the window labels overlay shows about one drawable surface, children included.
// Filled only on frames the scripts ask for it (yk.want_inspect).
pub struct Inspect {
    pub kind: &'static str,
    pub source: &'static str,
    pub tex_id: u32,
    pub tex_w: i32,
    pub tex_h: i32,
    pub sampling: String,
    pub canvas_x: f32,
    pub canvas_y: f32,
    pub geo_x: f32,
    pub geo_y: f32,
    pub geo_w: f32,
    pub geo_h: f32,
    pub z: f32,
    // The drawn rectangle, as the hit test sees it.
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

pub fn inspect(windows: &Windows, anisotropy: f32, out: &mut Vec<Inspect>) {
    out.clear();
    for i in 0..windows.surface.len() {
        if !drawable(windows, i) {
            continue;
        }
        let kind = if windows.popup[i] {
            "popup"
        } else if windows.sub[i] {
            "sub"
        } else {
            "window"
        };
        // What the sampler is actually set to, read from the column prepare_textures wrote,
        // with the reason when it is not the best available.
        let sampling = match windows.filter[i] {
            FILTER_NEAREST => "nearest".to_string(),
            FILTER_TRILINEAR => format!("trilinear+{anisotropy:.0}x aniso"),
            _ if windows.mip[i] == MIP_REFUSED && !windows.owns[i] => {
                "bilinear (no mips on dmabuf)".to_string()
            }
            _ if windows.mip[i] == MIP_REFUSED => "bilinear (driver refused mips)".to_string(),
            _ => "bilinear".to_string(),
        };
        let (x, y, w, h) = visible(windows, i);
        out.push(Inspect {
            kind,
            source: if windows.owns[i] { "shm" } else { "dmabuf" },
            tex_id: windows.tex_id[i],
            tex_w: windows.tex_w[i],
            tex_h: windows.tex_h[i],
            sampling,
            canvas_x: windows.canvas_x[i],
            canvas_y: windows.canvas_y[i],
            geo_x: windows.geo_x[i],
            geo_y: windows.geo_y[i],
            geo_w: windows.geo_w[i],
            geo_h: windows.geo_h[i],
            z: windows.z[i],
            x,
            y,
            w,
            h,
        });
    }
}

// The windows a script can see this frame: every drawable toplevel, by handle, with its drawn
// rectangle. Children are left out, since they are placed off their parents and are not
// anything a script would go to. Reused across frames, cleared and refilled by list_windows.
pub struct WinList {
    pub id: Vec<u64>,
    pub x: Vec<f32>,
    pub y: Vec<f32>,
    pub w: Vec<f32>,
    pub h: Vec<f32>,
    pub maximized: Vec<bool>,
    // Stack order: higher is in front.
    pub order: Vec<u32>,
    // How far the window is lifted toward the camera, as last set.
    pub z: Vec<f32>,
    // The size a maximized window goes back to, and 0 for one that is not maximized.
    pub restore_w: Vec<f32>,
    pub restore_h: Vec<f32>,
    // The size the client has committed, which is not the drawn size while stretched.
    pub geo_w: Vec<f32>,
    pub geo_h: Vec<f32>,
}

pub fn win_list_new() -> WinList {
    WinList {
        id: Vec::with_capacity(32),
        x: Vec::with_capacity(32),
        y: Vec::with_capacity(32),
        w: Vec::with_capacity(32),
        h: Vec::with_capacity(32),
        maximized: Vec::with_capacity(32),
        order: Vec::with_capacity(32),
        z: Vec::with_capacity(32),
        restore_w: Vec::with_capacity(32),
        restore_h: Vec::with_capacity(32),
        geo_w: Vec::with_capacity(32),
        geo_h: Vec::with_capacity(32),
    }
}

pub fn list_windows(windows: &Windows, out: &mut WinList) {
    out.id.clear();
    out.x.clear();
    out.y.clear();
    out.w.clear();
    out.h.clear();
    out.maximized.clear();
    out.order.clear();
    out.z.clear();
    out.restore_w.clear();
    out.restore_h.clear();
    out.geo_w.clear();
    out.geo_h.clear();
    for i in 0..windows.surface.len() {
        if child(windows, i) || !drawable(windows, i) {
            continue;
        }
        let (x, y, w, h) = visible(windows, i);
        out.id.push(windows.id[i]);
        out.x.push(x);
        out.y.push(y);
        out.w.push(w);
        out.h.push(h);
        out.maximized.push(windows.maximized[i]);
        out.order.push(windows.order[i]);
        out.z.push(windows.z[i]);
        let max = windows.maximized[i];
        out.restore_w.push(if max { windows.restore_w[i] } else { 0.0 });
        out.restore_h.push(if max { windows.restore_h[i] } else { 0.0 });
        out.geo_w.push(windows.geo_w[i]);
        out.geo_h.push(windows.geo_h[i]);
    }
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

// The size a resize is asking for, which a stretched window is shown at.
pub fn stretch_to(windows: &mut Windows, surface: &WlSurface, w: f32, h: f32) {
    if let Some(i) = index_of(windows, surface) {
        windows.shown_w[i] = w;
        windows.shown_h[i] = h;
        rescale(windows, i);
    }
}

// How big a window is on screen: the asked size while stretched, what the client committed
// otherwise.
pub fn shown_size(windows: &Windows, surface: &WlSurface) -> Option<(f32, f32)> {
    index_of(windows, surface)
        .map(|i| (windows.geo_w[i] * windows.scale_x[i], windows.geo_h[i] * windows.scale_y[i]))
}

fn rescale(windows: &mut Windows, i: usize) {
    let (gw, gh) = (windows.geo_w[i], windows.geo_h[i]);
    let (w, h) = (windows.shown_w[i], windows.shown_h[i]);
    if windows.stretch[i] && w > 0.0 && h > 0.0 && gw > 0.0 && gh > 0.0 {
        windows.scale_x[i] = (w / gw).max(0.01);
        windows.scale_y[i] = (h / gh).max(0.01);
    } else {
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

// The handle for a surface we hold, and the surface behind a handle. A handle whose window
// has gone answers None.
pub fn window_id(windows: &Windows, surface: &WlSurface) -> Option<u64> {
    index_of(windows, surface).map(|i| windows.id[i])
}

pub fn surface_by_id(windows: &Windows, id: u64) -> Option<WlSurface> {
    let i = windows.id.iter().position(|&x| x == id)?;
    Some(windows.surface[i].clone())
}

// Remove windows whose surface has died (client closed), freeing any shm
// texture we own. dmabuf textures belong to the cache and its buffer_destroyed
// path, so we do not touch them here.
pub fn prune_dead(windows: &mut Windows, closed: &mut Vec<u64>) {
    let mut i = 0;
    while i < windows.surface.len() {
        if windows.surface[i].is_alive() {
            i += 1;
            continue;
        }
        if windows.owns[i] && windows.tex_id[i] != 0 {
            ray::unload_texture(windows.tex_id[i]);
        }
        if !child(windows, i) {
            closed.push(windows.id[i]);
        }
        windows.surface.remove(i);
        windows.id.remove(i);
        windows.tex_id.remove(i);
        windows.tex_w.remove(i);
        windows.tex_h.remove(i);
        windows.canvas_x.remove(i);
        windows.canvas_y.remove(i);
        windows.draw_x.remove(i);
        windows.draw_y.remove(i);
        windows.z.remove(i);
        windows.order.remove(i);
        windows.geo_x.remove(i);
        windows.geo_y.remove(i);
        windows.geo_w.remove(i);
        windows.geo_h.remove(i);
        windows.swizzle.remove(i);
        windows.scale_x.remove(i);
        windows.scale_y.remove(i);
        windows.stretch.remove(i);
        windows.shown_w.remove(i);
        windows.shown_h.remove(i);
        windows.filter.remove(i);
        windows.alpha.remove(i);
        windows.shadow.remove(i);
        windows.shader.remove(i);
        windows.params.remove(i);
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

// How a window looks this frame, as the scripts say (yk.window_look): how opaque, how far it
// is lifted toward the camera (negative z is nearer), whether its shadow padding is drawn, and
// whether it is stretched to the size a resize is asking for.
// Any animation of these is the scripts' too; this only stores what they arrived at. Popups and
// subsurfaces take their parent's look when they are placed (sync_children).
// Which shader draws a window and the four numbers handed to it (yk.window_shader).
pub fn set_shader(windows: &mut Windows, surface: &WlSurface, slot: u8, params: [f32; 4]) {
    if let Some(i) = index_of(windows, surface) {
        windows.shader[i] = slot;
        windows.params[i] = params;
    }
}

// How far a window is lifted, 0 for one we do not hold.
pub fn window_lift(windows: &Windows, surface: &WlSurface) -> f32 {
    index_of(windows, surface).map(|i| windows.z[i]).unwrap_or(0.0)
}

pub fn set_look(
    windows: &mut Windows,
    surface: &WlSurface,
    alpha: f32,
    z: f32,
    shadow: bool,
    stretch: bool,
) {
    if let Some(i) = index_of(windows, surface) {
        windows.alpha[i] = alpha.clamp(0.0, 1.0);
        windows.z[i] = z;
        windows.shadow[i] = shadow;
        windows.stretch[i] = stretch;
        rescale(windows, i);
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

// Why the scripts are not running, across the top of the screen. A few lines at most: the
// first is where it went wrong, and a traceback past that belongs in the log.
pub fn draw_script_error(text: &str, screen_w: i32) {
    const PAD: i32 = 8;
    const TEXT: i32 = 16;
    const LINE: i32 = 20;
    const MAX_LINES: usize = 6;
    let bg = ray::Color { r: 60, g: 12, b: 16, a: 220 };
    let fg = ray::Color { r: 255, g: 190, b: 180, a: 255 };
    let count = text.lines().take(MAX_LINES).count() as i32;
    ray::draw_rectangle(0, 0, screen_w, count * LINE + PAD * 2, bg);
    for (i, line) in text.lines().take(MAX_LINES).enumerate() {
        ray::draw_text(line, PAD, PAD + i as i32 * LINE, TEXT, fg);
    }
}

// Dump every rect the hit test can pick, for when a click seems to land inside
// something and does not.
pub fn log_rects(windows: &Windows) {
    for i in 0..windows.surface.len() {
        println!(
            "yuka:   {} {}x{} at {:.0},{:.0} z={:.2} order={} placed={}",
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
//
// ime is the input method's candidate list, if one is showing: the popup, the window it
// belongs to, and its offset from that window's origin (state::ime_popup_placement).
pub fn sync_children(windows: &mut Windows, ime: Option<(WlSurface, WlSurface, f32, f32)>) {
    for i in 0..windows.surface.len() {
        windows.placed[i] = false;
    }
    let mut moves: Vec<(usize, f32, f32, f32, usize)> = Vec::new();
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
                    i,
                ));
            }
        }
    }
    for (j, x, y, z, parent) in moves {
        windows.canvas_x[j] = x;
        windows.canvas_y[j] = y;
        windows.z[j] = z;
        windows.order[j] = windows.order[parent];
        windows.alpha[j] = windows.alpha[parent];
        windows.shadow[j] = windows.shadow[parent];
        windows.shader[j] = windows.shader[parent];
        windows.params[j] = windows.params[parent];
        windows.placed[j] = true;
    }

    // Then every subsurface tree, from each root that is not itself a subsurface, so
    // a popup's children are placed off the popup we just moved. The tree comes back in the
    // client's stacking order with the root at its own slot.
    let mut subs: Vec<(usize, f32, f32, f32, usize)> = Vec::new();
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
                i,
            ));
        }
    }
    for (j, x, y, z, parent) in subs {
        windows.canvas_x[j] = x;
        windows.canvas_y[j] = y;
        windows.z[j] = z;
        windows.order[j] = windows.order[parent];
        windows.alpha[j] = windows.alpha[parent];
        windows.shadow[j] = windows.shadow[parent];
        windows.shader[j] = windows.shader[parent];
        windows.params[j] = windows.params[parent];
        windows.placed[j] = true;
    }

    // Last, under the text cursor and one step nearer than anything else of its window, menus
    // and subsurfaces included: the candidates are what you are reading while you type. Drawn
    // with the window's look but plain, since a window shader is about the window.
    let Some((popup, root, ox, oy)) = ime else { return };
    let (Some(i), Some(j)) = (index_of(windows, &root), index_of(windows, &popup)) else {
        return;
    };
    let mut nearest = windows.z[i];
    for k in 0..windows.surface.len() {
        if windows.placed[k] && windows.order[k] == windows.order[i] && windows.z[k] < nearest {
            nearest = windows.z[k];
        }
    }
    windows.canvas_x[j] = windows.canvas_x[i] + ox;
    windows.canvas_y[j] = windows.canvas_y[i] + oy;
    windows.z[j] = nearest - CHILD_LIFT;
    windows.order[j] = windows.order[i];
    windows.alpha[j] = windows.alpha[i];
    windows.shadow[j] = false;
    windows.shader[j] = 0;
    windows.params[j] = [0.0; 4];
    windows.placed[j] = true;
}

// Bring a window to the front of the stack (higher order draws on top).
pub fn front(windows: &mut Windows, surface: &WlSurface) {
    if let Some(i) = index_of(windows, surface) {
        windows.order[i] = windows.next_order;
        windows.next_order += 1;
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
            // Centre new windows on the view. Where a window actually goes is the scripts' to
            // decide, on the frame it maps and before it is first drawn; this is only where it
            // starts. Popups are not placed at all: sync_children puts them under their parent
            // before the first draw.
            //
            // Rounded, because centring a window with an odd dimension lands it on a half
            // unit, and a window off the pixel grid can never be drawn at 1:1 with exact
            // texels (see prepare_textures). The buffer's size, since geometry is not known
            // until the client's first commit lands.
            let (cx, cy) = if popup || sub {
                (0.0, 0.0)
            } else {
                (
                    (windows.place_at.0 - w as f32 * 0.5).round(),
                    (windows.place_at.1 - h as f32 * 0.5).round(),
                )
            };
            let order = windows.next_order;
            windows.next_order += 1;
            windows.surface.push(surface.clone());
            windows.id.push(windows.next_id);
            windows.next_id += 1;
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
            windows.order.push(order);
            windows.geo_x.push(0.0);
            windows.geo_y.push(0.0);
            windows.geo_w.push(w as f32);
            windows.geo_h.push(h as f32);
            windows.swizzle.push(swizzle);
            windows.scale_x.push(1.0);
            windows.scale_y.push(1.0);
            windows.stretch.push(false);
            windows.shown_w.push(0.0);
            windows.shown_h.push(0.0);
            windows.filter.push(FILTER_UNSET);
            windows.alpha.push(1.0);
            windows.shadow.push(true);
            windows.shader.push(0);
            windows.params.push([0.0; 4]);
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
                "yuka: {} + {w}x{h} at {cx:.0},{cy:.0}",
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
                "yuka: ZERO-COPY dmabuf import ok {}x{} fourcc={:#x} mod={:#x} gl_tex={tex}",
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
                "yuka: dmabuf import failed {}x{} fourcc={:#x} mod={:#x}",
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
    windows.order.clear();
    windows.geo_x.clear();
    windows.geo_y.clear();
    windows.geo_w.clear();
    windows.geo_h.clear();
    windows.swizzle.clear();
    windows.scale_x.clear();
    windows.scale_y.clear();
    windows.stretch.clear();
    windows.shown_w.clear();
    windows.shown_h.clear();
    windows.filter.clear();
    windows.alpha.clear();
    windows.shadow.clear();
    windows.shader.clear();
    windows.params.clear();
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
}

//
// Shaders
//
// Slots the scripts load window shaders into (yk.shader), by number. Slot 0 is the built-in
// shader until a script replaces it. Every window shader is handed the same uniforms, and
// declares whichever it uses:
//
//   sampler2D texture0, vec4 colDiffuse    raylib's own
//   float swizzleBgra                      1 for shm buffers (BGRA), 0 for dmabuf
//   float alphaBlend                       0; 1 forces opaque
//   float windowAlpha                      the window's opacity (yk.window_look)
//   vec4 windowParams                      the window's four numbers (yk.window_shader)
//   vec2 windowSize                        drawn size in canvas units
//   float time                             seconds since yuka started
//
// plus any the scripts set on the slot with yk.shader_uniform.
//

pub const SHADER_SLOTS: usize = 16;

pub struct ShaderSlot {
    pub shader: ray::Shader,
    alpha_loc: i32,
    swizzle_loc: i32,
    fade_loc: i32,
    params_loc: i32,
    size_loc: i32,
    time_loc: i32,
    // Only the floor sets this one: canvas to screen scale, for things meant to be a fixed
    // number of pixels on screen however far the view is zoomed.
    zoom_loc: i32,
    // Uniforms the scripts set on this slot: name, location, value, how many floats.
    globals: Vec<(String, i32, [f32; 4], u8)>,
}

pub struct Shaders {
    pub slots: Vec<Option<ShaderSlot>>,
}

fn slot_from(shader: ray::Shader) -> ShaderSlot {
    ShaderSlot {
        shader,
        alpha_loc: ray::shader_location(shader, "alphaBlend"),
        swizzle_loc: ray::shader_location(shader, "swizzleBgra"),
        fade_loc: ray::shader_location(shader, "windowAlpha"),
        params_loc: ray::shader_location(shader, "windowParams"),
        size_loc: ray::shader_location(shader, "windowSize"),
        time_loc: ray::shader_location(shader, "time"),
        zoom_loc: ray::shader_location(shader, "canvasZoom"),
        globals: Vec::new(),
    }
}

// The built-in shader in slot 0. It has to load: there is nothing to draw windows with
// otherwise.
pub fn shaders_new(vs: &str, fs: &str) -> Shaders {
    let mut slots: Vec<Option<ShaderSlot>> = Vec::with_capacity(SHADER_SLOTS);
    for _ in 0..SHADER_SLOTS {
        slots.push(None);
    }
    slots[0] = Some(slot_from(ray::load_shader(vs, fs)));
    Shaders { slots }
}

// Load a shader into a slot, replacing what was there. A shader that will not compile leaves
// the slot as it was, so a typo costs the effect and never the windows.
pub fn load_shader_slot(sh: &mut Shaders, slot: usize, vs: &str, fs: &str) -> Result<(), String> {
    if slot >= SHADER_SLOTS {
        return Err(format!("shader slot {slot} is past the last ({})", SHADER_SLOTS - 1));
    }
    let shader = ray::load_shader(vs, fs);
    if !ray::shader_loaded(shader) {
        return Err(format!("shader {vs} + {fs} did not compile, see the log"));
    }
    if let Some(old) = sh.slots[slot].take() {
        ray::unload_shader(old.shader);
    }
    println!("yuka: shader slot {slot}: {vs} + {fs}");
    sh.slots[slot] = Some(slot_from(shader));
    Ok(())
}

pub fn set_shader_uniform(sh: &mut Shaders, slot: usize, name: &str, values: &[f32]) {
    let Some(Some(s)) = sh.slots.get_mut(slot) else { return };
    let n = values.len().min(4);
    let mut v = [0.0f32; 4];
    v[..n].copy_from_slice(&values[..n]);
    if let Some(g) = s.globals.iter_mut().find(|g| g.0 == name) {
        g.2 = v;
        g.3 = n as u8;
        return;
    }
    let loc = ray::shader_location(s.shader, name);
    s.globals.push((name.to_string(), loc, v, n as u8));
}

pub fn shaders_destroy(sh: &mut Shaders) {
    for slot in sh.slots.iter_mut() {
        if let Some(s) = slot.take() {
            ray::unload_shader(s.shader);
        }
    }
}

//
// Draw
//

// The floor: the canvas plane itself, drawn behind every window by the shader slot the scripts
// chose for it. A quad at z 0 over the part of the canvas in view, with a margin, whose texture
// coordinates are canvas coordinates. So the shader is told where on the canvas each pixel is
// and the floor pans and zooms with the windows by construction; a lifted window moves off it
// toward the camera like it does off anything else at z 0.
//
// What the shader gets: frag_uv, the canvas point; canvasZoom, the canvas to screen scale; time;
// and whatever uniforms the scripts set on the slot. The texture is raylib's one white pixel.
pub fn draw_floor(
    shaders: &Shaders,
    slot: usize,
    cam3d: ray::Camera3D,
    view: (f32, f32, f32),
    screen: (f32, f32),
    time: f32,
) {
    let Some(Some(sl)) = shaders.slots.get(slot) else { return };
    let (cx, cy, zoom) = view;
    // Half the view in canvas units, and a tenth again, so a frame that pans further than the
    // camera says never shows an edge.
    let hw = screen.0 * 0.55 / zoom;
    let hh = screen.1 * 0.55 / zoom;
    let (x0, y0, x1, y1) = (cx - hw, cy - hh, cx + hw, cy + hh);
    let corners = [
        (Vector3 { x: x0, y: y0, z: 0.0 }, x0, y0),
        (Vector3 { x: x1, y: y0, z: 0.0 }, x1, y0),
        (Vector3 { x: x1, y: y1, z: 0.0 }, x1, y1),
        (Vector3 { x: x0, y: y1, z: 0.0 }, x0, y1),
    ];
    let shader = sl.shader;
    ray::begin_mode_3d(cam3d);
    ray::disable_backface_culling();
    ray::disable_depth_test();
    ray::begin_shader_mode(shader);
    if sl.zoom_loc >= 0 {
        ray::set_shader_float(shader, sl.zoom_loc, zoom);
    }
    if sl.time_loc >= 0 {
        ray::set_shader_float(shader, sl.time_loc, time);
    }
    for (_, loc, v, n) in sl.globals.iter() {
        if *loc >= 0 {
            ray::set_shader_vec(shader, *loc, &v[..*n as usize]);
        }
    }
    ray::draw_textured_quad(0, corners);
    ray::end_shader_mode();
    ray::enable_depth_test();
    ray::enable_backface_culling();
    ray::end_mode_3d();
}

pub fn draw_windows(windows: &Windows, cam3d: ray::Camera3D, shaders: &Shaders, time: f32) {
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
        let (x, y, w, h, u0, v0, u1, v1) = if windows.shadow[i] {
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
        // The window's own slot, or the built-in one when that slot is empty.
        let slot = shaders
            .slots
            .get(windows.shader[i] as usize)
            .and_then(|s| s.as_ref())
            .or(shaders.slots[0].as_ref());
        let Some(sl) = slot else { continue };
        let shader = sl.shader;
        ray::begin_shader_mode(shader);
        ray::set_shader_float(shader, sl.alpha_loc, 0.0);
        ray::set_shader_float(shader, sl.swizzle_loc, windows.swizzle[i]);
        ray::set_shader_float(shader, sl.fade_loc, windows.alpha[i]);
        if sl.params_loc >= 0 {
            ray::set_shader_vec(shader, sl.params_loc, &windows.params[i]);
        }
        if sl.size_loc >= 0 {
            ray::set_shader_vec(shader, sl.size_loc, &[w, h]);
        }
        if sl.time_loc >= 0 {
            ray::set_shader_float(shader, sl.time_loc, time);
        }
        for (_, loc, v, n) in sl.globals.iter() {
            if *loc >= 0 {
                ray::set_shader_vec(shader, *loc, &v[..*n as usize]);
            }
        }
        ray::draw_textured_quad(windows.tex_id[i], corners);
        ray::end_shader_mode();
    }
    ray::enable_depth_test();
    ray::enable_backface_culling();
    ray::end_mode_3d();
}
