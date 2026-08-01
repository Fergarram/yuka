//
// Cursor (Data Oriented zone)
//
// Hardware DRM cursor plane. We reuse the DRM master fd that raylib already
// opened (found via /proc/self/fd, so it is master in our process) and the
// active CRTC, upload a 64x64 ARGB arrow into a dumb buffer once, then move it
// with drmModeMoveCursor. The display controller composites it over our frame
// with no recomposite, so it is decoupled from the render content.
//
// All the libdrm FFI and raw pointer work is contained here.
//

use std::fs;
use std::path::Path;

//
// Constants
//

const CURSOR_SIZE: u32 = 64;
// Our own cursor: a crosshair centered at (10, 10), so its hotspot is there.
const CROSSHAIR_HOT_X: i32 = 10;
const CROSSHAIR_HOT_Y: i32 = 10;

//
// libdrm FFI
//

#[repr(C)]
struct DrmModeRes {
    count_fbs: i32,
    fbs: *mut u32,
    count_crtcs: i32,
    crtcs: *mut u32,
    count_connectors: i32,
    connectors: *mut u32,
    count_encoders: i32,
    encoders: *mut u32,
    min_width: u32,
    max_width: u32,
    min_height: u32,
    max_height: u32,
}

// Prefix of drmModeCrtc; we only read crtc_id and buffer_id via the pointer.
#[repr(C)]
struct DrmModeCrtc {
    crtc_id: u32,
    buffer_id: u32,
}

extern "C" {
    fn drmModeGetResources(fd: i32) -> *mut DrmModeRes;
    fn drmModeFreeResources(ptr: *mut DrmModeRes);
    fn drmModeGetCrtc(fd: i32, crtc_id: u32) -> *mut DrmModeCrtc;
    fn drmModeFreeCrtc(ptr: *mut DrmModeCrtc);
    fn drmModeCreateDumbBuffer(
        fd: i32,
        width: u32,
        height: u32,
        bpp: u32,
        flags: u32,
        handle: *mut u32,
        pitch: *mut u32,
        size: *mut u64,
    ) -> i32;
    fn drmModeMapDumbBuffer(fd: i32, handle: u32, offset: *mut u64) -> i32;
    fn drmModeDestroyDumbBuffer(fd: i32, handle: u32) -> i32;
    fn drmModeSetCursor2(
        fd: i32,
        crtc_id: u32,
        bo_handle: u32,
        width: u32,
        height: u32,
        hot_x: i32,
        hot_y: i32,
    ) -> i32;
    fn drmModeMoveCursor(fd: i32, crtc_id: u32, x: i32, y: i32) -> i32;
}

//
// Types
//

pub struct Cursor {
    fd: i32,
    crtc: u32,
    handle: u32,
    x: i32,
    y: i32,
    max_x: i32,
    max_y: i32,
    // The dumb buffer stays mapped for the process, because a client changes its cursor
    // image whenever the pointer crosses into a text field or a resize edge, and mapping
    // it again for every one of those would be a syscall pair per shape.
    map: *mut u8,
    pitch: u32,
    size: u64,
    // The hotspot currently in force, which is the crosshair's or the client's. Every move
    // is offset by it, so the pointer position and the point of the arrow are the same
    // place: this is what decides whether a click lands where the cursor looks like it is.
    hot_x: i32,
    hot_y: i32,
    // Whether the plane is armed at all. A client may ask for no cursor.
    visible: bool,
    // What is uploaded right now, so an unchanged cursor is not re-uploaded every frame.
    shape: Shape,
    // Our own copy of the client's cursor image, and where its hotspot is.
    //
    // Kept because the client's buffer is not ours to hold: it is released back the moment
    // we have read it, and the plane's buffer gets overwritten whenever the crosshair comes
    // back. Without a copy, crossing from the canvas onto a window would show the crosshair
    // until the client happened to commit its cursor again, which it has no reason to do.
    client: Vec<u32>,
    client_hot: (i32, i32),
    // Bumped when a new client image is stored, so an unchanged one is not re-blitted, and
    // a fingerprint of those pixels so an identical image is not stored at all. Clients
    // re-set their cursor constantly (Chromium does it every few frames while dragging),
    // and each of those used to mean a full re-bind of the plane.
    client_gen: u32,
    client_hash: u64,
    has_client: bool,
    debug: bool,
    // Where the plane has been told to sit, as opposed to where the pointer logically is.
    //
    // The two are separate because the plane is not part of our frame: drmModeMoveCursor
    // takes effect at once, over whatever the display is already scanning out, while a
    // window being dragged only moves when the next frame is presented. Moving the plane
    // the moment the pointer moved therefore ran the cursor up to a frame ahead of the
    // window locked to it, which at speed looks like the grab point sliding out from under
    // the cursor and snapping back when you stop.
    shown_x: i32,
    shown_y: i32,
    // How long the plane's move ioctl actually takes: a cursor that lags while the window
    // it drags does not is either being told late, or being told slowly.
    move_calls: u64,
    move_ms: f64,
    // Whether to hold the plane back until the frame is about to be presented.
    //
    // The plane updates the instant the ioctl lands, while our windows only appear at the
    // next flip. Both are computed from the same input sample, so they agree about where the
    // pointer is, but the cursor gets there a frame earlier. That is invisible until a window
    // is following the cursor, when it reads as the window trailing and catching up. While
    // that is happening the move waits for present_deferred, just before the flip, and the
    // two land together.
    deferred: bool,
}

// Which image the plane is carrying.
#[derive(Clone, Copy, PartialEq)]
pub enum Shape {
    Crosshair,
    Hidden,
    // A client's own image. The generation counter changes whenever that surface commits
    // new pixels, which is how an animated cursor gets uploaded again.
    Client(u32),
}

//
// Init
//

pub fn init(screen_w: i32, screen_h: i32) -> Option<Cursor> {
    let fd = find_drm_fd()?;
    let crtc = find_active_crtc(fd)?;

    let mut handle: u32 = 0;
    let mut pitch: u32 = 0;
    let mut size: u64 = 0;
    let r = unsafe {
        drmModeCreateDumbBuffer(
            fd,
            CURSOR_SIZE,
            CURSOR_SIZE,
            32,
            0,
            &mut handle,
            &mut pitch,
            &mut size,
        )
    };
    if r != 0 {
        eprintln!("om_wm: cursor dumb buffer create failed");
        return None;
    }

    let mut offset: u64 = 0;
    if unsafe { drmModeMapDumbBuffer(fd, handle, &mut offset) } != 0 {
        return None;
    }
    let map = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            size as usize,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd,
            offset as libc::off_t,
        )
    };
    if map == libc::MAP_FAILED {
        return None;
    }

    draw_crosshair(map as *mut u8, pitch);

    if unsafe {
        drmModeSetCursor2(
            fd,
            crtc,
            handle,
            CURSOR_SIZE,
            CURSOR_SIZE,
            CROSSHAIR_HOT_X,
            CROSSHAIR_HOT_Y,
        )
    } != 0
    {
        eprintln!("om_wm: drmModeSetCursor2 failed");
        return None;
    }

    println!("om_wm: hardware cursor on crtc {crtc} (drm fd {fd})");
    let mut c = Cursor {
        fd,
        crtc,
        handle,
        x: screen_w / 2,
        y: screen_h / 2,
        max_x: screen_w,
        max_y: screen_h,
        map: map as *mut u8,
        pitch,
        size,
        hot_x: CROSSHAIR_HOT_X,
        hot_y: CROSSHAIR_HOT_Y,
        visible: true,
        shape: Shape::Crosshair,
        client: vec![0; (CURSOR_SIZE * CURSOR_SIZE) as usize],
        client_hot: (0, 0),
        client_gen: 0,
        client_hash: 0,
        has_client: false,
        debug: std::env::var("OM_WM_DEBUG_INPUT").is_ok(),
        shown_x: i32::MIN,
        shown_y: i32::MIN,
        move_calls: 0,
        move_ms: 0.0,
        deferred: false,
    };
    let (x, y) = (c.x, c.y);
    move_to(&mut c, x, y);
    Some(c)
}

pub fn move_by(c: &mut Cursor, dx: i32, dy: i32) {
    let nx = (c.x + dx).clamp(0, c.max_x - 1);
    let ny = (c.y + dy).clamp(0, c.max_y - 1);
    move_to(c, nx, ny);
}

// c.x/c.y is the logical pointer position: the hotspot, and what everything that reads the
// pointer reads. Moving it moves the plane at once.
//
// At once, deliberately. This was briefly deferred to just after the flip, on the theory
// that the plane was running ahead of the window being dragged with it. It is the other way
// round: the move takes effect at the next vblank, so issuing it after the flip lands it one
// whole frame behind the frame it belongs to, which at drag speed is fifteen pixels of the
// cursor smoothly trailing the window. Issued here, both reach the same vblank.
fn move_to(c: &mut Cursor, x: i32, y: i32) {
    c.x = x;
    c.y = y;
    if !c.deferred {
        place(c);
    }
}

// Hold the plane back to present time, or stop doing so. On for as long as something on the
// canvas is following the pointer.
pub fn set_deferred(c: &mut Cursor, deferred: bool) {
    c.deferred = deferred;
    if !deferred {
        place(c);
    }
}

// Move the plane now, for the caller that knows the frame is about to be presented.
pub fn present_deferred(c: &mut Cursor) {
    if c.deferred {
        place(c);
    }
}

// Put the plane where the pointer is, unless it is already there. The plane's top-left goes
// hotspot-offset back, so the hotspot lands exactly on the pointer position.
fn place(c: &mut Cursor) {
    if c.x == c.shown_x && c.y == c.shown_y {
        return;
    }
    c.shown_x = c.x;
    c.shown_y = c.y;
    let t0 = std::time::Instant::now();
    let r = unsafe { drmModeMoveCursor(c.fd, c.crtc, c.x - c.hot_x, c.y - c.hot_y) };
    if r != 0 {
        // A plane that will not move is invisible except as a cursor that lags, so it is
        // worth saying rather than discarding.
        eprintln!("om_wm: cursor move failed ({r})");
    }
    if c.debug {
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        c.move_calls += 1;
        c.move_ms += ms;
        if ms > 2.0 {
            println!("om_wm: cursor move took {ms:.2}ms");
        }
        if c.move_calls % 120 == 0 {
            println!(
                "om_wm: cursor moves {} avg {:.3}ms",
                c.move_calls,
                c.move_ms / c.move_calls as f64
            );
        }
    }
}

//
// Shapes
//

// Back to our own crosshair, for the canvas.
pub fn set_crosshair(c: &mut Cursor) {
    if c.shape == Shape::Crosshair {
        return;
    }
    clear(c);
    draw_crosshair(c.map, c.pitch);
    c.shape = Shape::Crosshair;
    arm(c, CROSSHAIR_HOT_X, CROSSHAIR_HOT_Y);
}

// No cursor at all, which a client is allowed to ask for: a video player hiding it, or a
// game drawing its own.
pub fn set_hidden(c: &mut Cursor) {
    if c.shape == Shape::Hidden {
        return;
    }
    c.shape = Shape::Hidden;
    c.visible = false;
    // A null handle is how the plane is switched off.
    unsafe { drmModeSetCursor2(c.fd, c.crtc, 0, 0, 0, 0, 0) };
}

// Take a copy of a client's cursor image, at the moment its buffer is still attached.
//
// The plane is a fixed 64x64 ARGB8888 and a client's cursor is usually 24 or 32 across, so
// this crops rather than scales: a cursor larger than the plane would need a scaled blit or
// a composited quad, and cropping keeps the hotspot exact, which matters more.
pub fn store_client_image(
    c: &mut Cursor,
    width: i32,
    height: i32,
    stride: i32,
    pixels: *const u8,
    hot_x: i32,
    hot_y: i32,
) {
    if width <= 0 || height <= 0 || pixels.is_null() {
        return;
    }
    c.client.iter_mut().for_each(|p| *p = 0);
    let rows = height.min(CURSOR_SIZE as i32) as usize;
    let cols = width.min(CURSOR_SIZE as i32) as usize;
    for y in 0..rows {
        let src = unsafe { pixels.add(y * stride as usize) as *const u32 };
        let dst = &mut c.client[y * CURSOR_SIZE as usize..];
        // Both sides are 32 bits per pixel in the same order (shm ARGB8888 and the plane's
        // ARGB8888), so this is a straight row copy.
        unsafe { std::ptr::copy_nonoverlapping(src, dst.as_mut_ptr(), cols) };
    }

    // The same image again, which is most of what clients send: nothing to do.
    let hash = fingerprint(&c.client);
    let showing = matches!(c.shape, Shape::Client(_));
    if c.has_client && hash == c.client_hash && (hot_x, hot_y) == c.client_hot && showing {
        return;
    }
    let hotspot_moved = (hot_x, hot_y) != c.client_hot;
    c.client_hash = hash;
    c.client_hot = (hot_x, hot_y);
    c.client_gen = c.client_gen.wrapping_add(1);
    c.has_client = true;

    if !showing {
        return;
    }
    // Already showing this client's cursor: the new pixels go straight into the buffer the
    // CRTC is already scanning, which needs no ioctl at all. Only a moved hotspot does,
    // because it changes where the plane has to sit for the same pointer position.
    blit_client(c);
    c.shape = Shape::Client(c.client_gen);
    if hotspot_moved {
        arm(c, hot_x, hot_y);
    }
}

// Cheap fingerprint of the cursor pixels, to tell a repeat from a change. FNV-1a: no
// dependency, and a wrong answer only costs a re-upload nobody can see.
fn fingerprint(pixels: &[u32]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &p in pixels {
        h ^= p as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

pub fn has_client_image(c: &Cursor) -> bool {
    c.has_client
}

// Put the stored client image on the plane.
pub fn apply_client(c: &mut Cursor) {
    if !c.has_client || c.shape == Shape::Client(c.client_gen) {
        return;
    }
    let coming_from_elsewhere = !matches!(c.shape, Shape::Client(_));
    blit_client(c);
    c.shape = Shape::Client(c.client_gen);
    let (hx, hy) = c.client_hot;
    // Coming from the crosshair or from hidden, the plane has to be handed the buffer and
    // the hotspot again. Between two client images it does not.
    if coming_from_elsewhere || (hx, hy) != (c.hot_x, c.hot_y) {
        arm(c, hx, hy);
    }
}

fn blit_client(c: &mut Cursor) {
    for y in 0..CURSOR_SIZE as usize {
        let src = &c.client[y * CURSOR_SIZE as usize..(y + 1) * CURSOR_SIZE as usize];
        let dst = unsafe { c.map.add(y * c.pitch as usize) as *mut u32 };
        unsafe { std::ptr::copy_nonoverlapping(src.as_ptr(), dst, CURSOR_SIZE as usize) };
    }
}

fn clear(c: &mut Cursor) {
    unsafe { std::ptr::write_bytes(c.map, 0, c.size as usize) };
}

// Hand the plane its buffer and hotspot, and put it back where the pointer is: the offset
// a move applies depends on the hotspot, so changing one means redoing the other.
fn arm(c: &mut Cursor, hot_x: i32, hot_y: i32) {
    if c.debug {
        println!(
            "om_wm: cursor armed hotspot {hot_x},{hot_y} was {},{} at {},{}",
            c.hot_x, c.hot_y, c.x, c.y
        );
    }
    c.hot_x = hot_x;
    c.hot_y = hot_y;
    let r = unsafe {
        drmModeSetCursor2(c.fd, c.crtc, c.handle, CURSOR_SIZE, CURSOR_SIZE, hot_x, hot_y)
    };
    if r != 0 {
        eprintln!("om_wm: cursor set image failed");
        return;
    }
    c.visible = true;
    // A new hotspot changes where the plane sits for the same pointer position, so this one
    // cannot wait for the next frame.
    c.shown_x = i32::MIN;
    place(c);
}

pub fn pos(c: &Cursor) -> (i32, i32) {
    (c.x, c.y)
}

// Re-arm the cursor plane. Dropping DRM master for a VT switch loses it: the
// console modesets the CRTC without a cursor, so we have to hand the plane back
// its buffer and position when we take the display again.
pub fn rearm(c: &mut Cursor) {
    if c.shape == Shape::Hidden {
        unsafe { drmModeSetCursor2(c.fd, c.crtc, 0, 0, 0, 0, 0) };
        return;
    }
    let (hot_x, hot_y) = (c.hot_x, c.hot_y);
    let r = unsafe {
        drmModeSetCursor2(c.fd, c.crtc, c.handle, CURSOR_SIZE, CURSOR_SIZE, hot_x, hot_y)
    };
    if r != 0 {
        eprintln!("om_wm: cursor re-arm failed");
        return;
    }
    c.shown_x = i32::MIN;
    place(c);
}

pub fn destroy(c: &mut Cursor) {
    unsafe {
        drmModeSetCursor2(c.fd, c.crtc, 0, 0, 0, 0, 0);
        drmModeDestroyDumbBuffer(c.fd, c.handle);
    }
}

//
// DRM discovery
//

// raylib opened /dev/dri/card0 in this process, so that fd is DRM master.
fn find_drm_fd() -> Option<i32> {
    let card = Path::new("/dev/dri/card0");
    for entry in fs::read_dir("/proc/self/fd").ok()? {
        let entry = entry.ok()?;
        if let Ok(target) = fs::read_link(entry.path()) {
            if target == card {
                if let Some(n) =
                    entry.file_name().to_str().and_then(|s| s.parse::<i32>().ok())
                {
                    return Some(n);
                }
            }
        }
    }
    eprintln!("om_wm: could not find raylib's DRM fd");
    None
}

// The CRTC currently scanning out a framebuffer (the one raylib set a mode on).
fn find_active_crtc(fd: i32) -> Option<u32> {
    let res = unsafe { drmModeGetResources(fd) };
    if res.is_null() {
        return None;
    }
    let r = unsafe { &*res };
    let crtcs =
        unsafe { std::slice::from_raw_parts(r.crtcs, r.count_crtcs.max(0) as usize) };

    let mut found = None;
    for &cid in crtcs {
        let cptr = unsafe { drmModeGetCrtc(fd, cid) };
        if !cptr.is_null() {
            let active = unsafe { (*cptr).buffer_id } != 0;
            let id = unsafe { (*cptr).crtc_id };
            unsafe { drmModeFreeCrtc(cptr) };
            if active {
                found = Some(id);
                break;
            }
        }
    }
    unsafe { drmModeFreeResources(res) };
    found
}

//
// Cursor bitmap
//

fn put(base: *mut u8, pitch: u32, x: i32, y: i32, color: u32) {
    if x < 0 || y < 0 || x >= CURSOR_SIZE as i32 || y >= CURSOR_SIZE as i32 {
        return;
    }
    let off = (y as u32 * pitch + x as u32 * 4) as usize;
    unsafe { *(base.add(off) as *mut u32) = color };
}

// A crosshair centered at (10, 10): white 1px arms with a black outline and a
// small center gap for precision. ARGB8888 (little-endian B,G,R,A).
fn draw_crosshair(base: *mut u8, pitch: u32) {
    const TRANSPARENT: u32 = 0x0000_0000;
    const WHITE: u32 = 0xFFFF_FFFF;
    const BLACK: u32 = 0xFF00_0000;

    for y in 0..CURSOR_SIZE as i32 {
        for x in 0..CURSOR_SIZE as i32 {
            put(base, pitch, x, y, TRANSPARENT);
        }
    }

    let c = 10i32;
    let len = 10i32;
    let gap = 2i32;

    // Black outline pass (3px arms), then white center pass (1px arms) on top.
    for &(color, thick) in &[(BLACK, 1i32), (WHITE, 0i32)] {
        for i in -len..=len {
            if i.abs() < gap {
                continue;
            }
            for w in -thick..=thick {
                put(base, pitch, c + i, c + w, color); // horizontal arm
                put(base, pitch, c + w, c + i, color); // vertical arm
            }
        }
    }
}
