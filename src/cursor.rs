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
// Default cursor: a crosshair centered at (10, 10); hotspot at its center.
const CURSOR_HOT_X: i32 = 10;
const CURSOR_HOT_Y: i32 = 10;

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
    unsafe { libc::munmap(map, size as usize) };

    if unsafe {
        drmModeSetCursor2(
            fd,
            crtc,
            handle,
            CURSOR_SIZE,
            CURSOR_SIZE,
            CURSOR_HOT_X,
            CURSOR_HOT_Y,
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

// c.x/c.y is the logical pointer position (the hotspot). The plane's top-left is
// placed hotspot-offset back so the hotspot lands exactly at (x, y).
fn move_to(c: &mut Cursor, x: i32, y: i32) {
    c.x = x;
    c.y = y;
    unsafe {
        drmModeMoveCursor(c.fd, c.crtc, x - CURSOR_HOT_X, y - CURSOR_HOT_Y)
    };
}

pub fn pos(c: &Cursor) -> (i32, i32) {
    (c.x, c.y)
}

// Re-arm the cursor plane. Dropping DRM master for a VT switch loses it: the
// console modesets the CRTC without a cursor, so we have to hand the plane back
// its buffer and position when we take the display again.
pub fn rearm(c: &mut Cursor) {
    let r = unsafe {
        drmModeSetCursor2(
            c.fd,
            c.crtc,
            c.handle,
            CURSOR_SIZE,
            CURSOR_SIZE,
            CURSOR_HOT_X,
            CURSOR_HOT_Y,
        )
    };
    if r != 0 {
        eprintln!("om_wm: cursor re-arm failed");
        return;
    }
    let (x, y) = (c.x, c.y);
    move_to(c, x, y);
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
