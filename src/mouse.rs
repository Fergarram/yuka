//
// Mouse (Data Oriented zone)
//
// Reads an external relative pointing device (a USB mouse) from evdev: relative
// motion moves the cursor, BTN_LEFT is a click, and the wheel zooms the canvas.
// Distinct from the trackpad (touch.rs), which is absolute multitouch.
//
// All raw fd/read work is contained here.
//

use std::ffi::{c_void, CString};
use std::fs;

//
// Constants (evdev)
//

const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const REL_X: u16 = 0x00;
const REL_Y: u16 = 0x01;
const REL_WHEEL: u16 = 0x08;
const BTN_LEFT: u16 = 0x110;
const BTN_MIDDLE: u16 = 0x112;
const EVENT_SIZE: usize = 24;
// EVIOCGRAB = _IOW('E', 0x90, int): grab a device exclusively.
const EVIOCGRAB: libc::c_ulong = 0x4004_4590;

//
// Types
//

pub struct Mouse {
    fd: i32,
    // Middle-button level, persisted across frames for drag-to-pan.
    middle: bool,
}

// Accumulated input since the last poll.
pub struct Frame {
    pub dx: i32,
    pub dy: i32,
    pub wheel: i32,
    pub pressed: bool,
    pub released: bool,
    // Middle button currently held (wheel-click), for drag-to-pan.
    pub middle: bool,
}

//
// Open
//

pub fn open() -> Option<Mouse> {
    let path = find_mouse()?;
    let c = CString::new(path.clone()).ok()?;
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_NONBLOCK) };
    if fd < 0 {
        eprintln!("om_wm: mouse open failed: {path}");
        return None;
    }
    unsafe { libc::ioctl(fd, EVIOCGRAB, 1 as libc::c_int) };
    println!("om_wm: mouse {path}");
    Some(Mouse { fd, middle: false })
}

// A device with non-zero relative axes (REL) that is not the trackpad.
fn find_mouse() -> Option<String> {
    let text = fs::read_to_string("/proc/bus/input/devices").ok()?;
    for block in text.split("\n\n") {
        let mut has_rel = false;
        let mut is_trackpad = false;
        let mut event: Option<String> = None;
        for line in block.lines() {
            if let Some(name) = line.strip_prefix("N: Name=") {
                let low = name.to_lowercase();
                is_trackpad = low.contains("bcm5974")
                    || low.contains("trackpad")
                    || low.contains("touchpad");
            }
            if let Some(rel) = line.strip_prefix("B: REL=") {
                has_rel = rel.split_whitespace().any(|h| {
                    u64::from_str_radix(h, 16).map(|v| v != 0).unwrap_or(false)
                });
            }
            if let Some(handlers) = line.strip_prefix("H: Handlers=") {
                for tok in handlers.split_whitespace() {
                    if let Some(n) = tok.strip_prefix("event") {
                        event = Some(n.to_string());
                    }
                }
            }
        }
        if has_rel && !is_trackpad {
            if let Some(n) = event {
                return Some(format!("/dev/input/event{n}"));
            }
        }
    }
    None
}

//
// Poll
//

pub fn poll(m: &mut Mouse) -> Frame {
    let mut frame =
        Frame { dx: 0, dy: 0, wheel: 0, pressed: false, released: false, middle: false };
    let mut buf = [0u8; EVENT_SIZE];
    loop {
        let n = unsafe {
            libc::read(m.fd, buf.as_mut_ptr() as *mut c_void, EVENT_SIZE)
        };
        if n != EVENT_SIZE as isize {
            break;
        }
        let etype = u16::from_ne_bytes([buf[16], buf[17]]);
        let code = u16::from_ne_bytes([buf[18], buf[19]]);
        let value = i32::from_ne_bytes([buf[20], buf[21], buf[22], buf[23]]);
        match (etype, code) {
            (EV_REL, REL_X) => frame.dx += value,
            (EV_REL, REL_Y) => frame.dy += value,
            (EV_REL, REL_WHEEL) => frame.wheel += value,
            (EV_KEY, BTN_LEFT) => {
                if value == 1 {
                    frame.pressed = true;
                } else if value == 0 {
                    frame.released = true;
                }
            }
            (EV_KEY, BTN_MIDDLE) => m.middle = value != 0,
            _ => {}
        }
    }
    frame.middle = m.middle;
    frame
}
