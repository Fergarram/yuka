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

use crate::kbd;

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

// Every relative pointing device, merged, for the same reason as the keyboard: a
// dongle can present several interfaces and picking one by name gets it wrong.
pub struct Mouse {
    fds: Vec<i32>,
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
    let mut fds: Vec<i32> = Vec::new();
    for (path, name) in find_mice() {
        let Ok(c) = CString::new(path.clone()) else { continue };
        let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_NONBLOCK) };
        if fd < 0 {
            eprintln!("om_wm: mouse open failed: {path}");
            continue;
        }
        println!("om_wm: mouse {path} ({name})");
        fds.push(fd);
    }
    if fds.is_empty() {
        return None;
    }
    Some(Mouse { fds, middle: false })
}

// Devices carrying both relative axes, which is what a pointer has, minus the
// trackpad (touch.rs drives that one as absolute multitouch).
fn find_mice() -> Vec<(String, String)> {
    let mut found: Vec<(String, String)> = Vec::new();
    let Ok(text) = fs::read_to_string("/proc/bus/input/devices") else { return found };
    for block in text.split("\n\n") {
        let mut name = String::new();
        let mut is_trackpad = false;
        let mut event: Option<String> = None;
        let mut rel: Vec<u64> = Vec::new();
        for line in block.lines() {
            if let Some(n) = line.strip_prefix("N: Name=") {
                name = n.trim_matches('"').to_string();
                let low = name.to_lowercase();
                is_trackpad = low.contains("bcm5974")
                    || low.contains("trackpad")
                    || low.contains("touchpad");
            }
            if let Some(bits) = line.strip_prefix("B: REL=") {
                rel = kbd::bitmap(bits);
            }
            if let Some(handlers) = line.strip_prefix("H: Handlers=") {
                for tok in handlers.split_whitespace() {
                    if let Some(n) = tok.strip_prefix("event") {
                        event = Some(n.to_string());
                    }
                }
            }
        }
        let points = kbd::bit_set(&rel, REL_X) && kbd::bit_set(&rel, REL_Y);
        if points && !is_trackpad {
            if let Some(n) = event {
                found.push((format!("/dev/input/event{n}"), name));
            }
        }
    }
    found
}

//
// Poll
//

pub fn poll(m: &mut Mouse) -> Frame {
    let mut frame =
        Frame { dx: 0, dy: 0, wheel: 0, pressed: false, released: false, middle: false };
    let mut buf = [0u8; EVENT_SIZE];
    for i in 0..m.fds.len() {
        let fd = m.fds[i];
        loop {
            let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut c_void, EVENT_SIZE) };
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
    }
    frame.middle = m.middle;
    frame
}

//
// Grab
//

// Hold or release the exclusive grab (only the no-session fallback takes it).
pub fn set_grab(m: &mut Mouse, on: bool) {
    let arg: libc::c_int = if on { 1 } else { 0 };
    for i in 0..m.fds.len() {
        unsafe { libc::ioctl(m.fds[i], EVIOCGRAB, arg) };
    }
}

// Drop queued events and button state after coming back from another VT.
pub fn reset(m: &mut Mouse) {
    let _ = poll(m);
    m.middle = false;
}
