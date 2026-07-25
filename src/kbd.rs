//
// Keyboard (Data Oriented zone)
//
// Reads the keyboard's raw evdev key events directly and tracks key state by
// stable Linux keycode. raylib's DRM keymap mismaps some keys (e.g. left Meta),
// so we bypass it for compositor shortcuts. This is also the foundation for
// forwarding keys to clients later.
//
// All raw fd/read work is contained here.
//

use std::ffi::{c_void, CString};
use std::fs;

//
// Constants
//

const EV_KEY: u16 = 0x01;
const EVENT_SIZE: usize = 24;
const KEY_ARRAY: usize = 768;
// EVIOCGRAB = _IOW('E', 0x90, int): grab a device exclusively.
const EVIOCGRAB: libc::c_ulong = 0x4004_4590;

// Linux evdev keycodes (input-event-codes.h) we care about.
pub const KEY_ESC: u16 = 1;
pub const KEY_MINUS: u16 = 12;
pub const KEY_EQUAL: u16 = 13;
pub const KEY_W: u16 = 17;
pub const KEY_A: u16 = 30;
pub const KEY_S: u16 = 31;
pub const KEY_D: u16 = 32;
pub const KEY_KPMINUS: u16 = 74;
pub const KEY_KPPLUS: u16 = 78;
pub const KEY_LEFTMETA: u16 = 125;
pub const KEY_RIGHTMETA: u16 = 126;

//
// Types
//

pub struct Keyboard {
    fd: i32,
    keys: Vec<bool>,
    // Press/release edges this frame: (evdev keycode, pressed). Repeat is skipped.
    events: Vec<(u16, bool)>,
}

//
// Open
//

pub fn open() -> Option<Keyboard> {
    let path = find_keyboard()?;
    let c = CString::new(path.clone()).ok()?;
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_NONBLOCK) };
    if fd < 0 {
        eprintln!("om_wm: keyboard open failed: {path}");
        return None;
    }
    // Grab exclusively so keystrokes do not also reach the tty console.
    unsafe { libc::ioctl(fd, EVIOCGRAB, 1 as libc::c_int) };
    println!("om_wm: keyboard {path}");
    Some(Keyboard { fd, keys: vec![false; KEY_ARRAY], events: Vec::new() })
}

// A device named like a keyboard that exposes the "kbd" handler.
fn find_keyboard() -> Option<String> {
    let text = fs::read_to_string("/proc/bus/input/devices").ok()?;
    for block in text.split("\n\n") {
        let mut is_kbd = false;
        let mut has_kbd_handler = false;
        let mut event: Option<String> = None;
        for line in block.lines() {
            if let Some(name) = line.strip_prefix("N: Name=") {
                is_kbd = name.to_lowercase().contains("keyboard");
            }
            if let Some(handlers) = line.strip_prefix("H: Handlers=") {
                for tok in handlers.split_whitespace() {
                    if tok == "kbd" {
                        has_kbd_handler = true;
                    }
                    if let Some(n) = tok.strip_prefix("event") {
                        event = Some(n.to_string());
                    }
                }
            }
        }
        if is_kbd && has_kbd_handler {
            if let Some(n) = event {
                return Some(format!("/dev/input/event{n}"));
            }
        }
    }
    None
}

//
// Poll / query
//

pub fn poll(kb: &mut Keyboard) {
    kb.events.clear();
    let mut buf = [0u8; EVENT_SIZE];
    loop {
        let n = unsafe {
            libc::read(kb.fd, buf.as_mut_ptr() as *mut c_void, EVENT_SIZE)
        };
        if n != EVENT_SIZE as isize {
            break;
        }
        let etype = u16::from_ne_bytes([buf[16], buf[17]]);
        if etype != EV_KEY {
            continue;
        }
        let code = u16::from_ne_bytes([buf[18], buf[19]]);
        let value = i32::from_ne_bytes([buf[20], buf[21], buf[22], buf[23]]);
        if (code as usize) < kb.keys.len() {
            // value 0 = up, 1 = down, 2 = repeat.
            kb.keys[code as usize] = value != 0;
        }
        // Forward only real press/release edges (not autorepeat).
        if value == 0 || value == 1 {
            kb.events.push((code, value == 1));
        }
    }
}

pub fn down(kb: &Keyboard, code: u16) -> bool {
    kb.keys.get(code as usize).copied().unwrap_or(false)
}

pub fn events(kb: &Keyboard) -> &[(u16, bool)] {
    &kb.events
}

pub fn super_down(kb: &Keyboard) -> bool {
    down(kb, KEY_LEFTMETA) || down(kb, KEY_RIGHTMETA)
}
