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
pub const KEY_BACKSPACE: u16 = 14;
pub const KEY_W: u16 = 17;
pub const KEY_LEFTCTRL: u16 = 29;
pub const KEY_A: u16 = 30;
pub const KEY_S: u16 = 31;
pub const KEY_D: u16 = 32;
pub const KEY_LEFTALT: u16 = 56;
pub const KEY_SPACE: u16 = 57;
pub const KEY_F1: u16 = 59;
pub const KEY_F10: u16 = 68;
pub const KEY_KPMINUS: u16 = 74;
pub const KEY_KPPLUS: u16 = 78;
pub const KEY_F11: u16 = 87;
pub const KEY_F12: u16 = 88;
pub const KEY_RIGHTCTRL: u16 = 97;
pub const KEY_RIGHTALT: u16 = 100;
pub const KEY_LEFT: u16 = 105;
pub const KEY_RIGHT: u16 = 106;
pub const KEY_LEFTMETA: u16 = 125;
pub const KEY_RIGHTMETA: u16 = 126;

//
// Types
//

// Every keyboard on the machine, merged. One device is not enough: a laptop with
// an external keyboard plugged in has several, and a name heuristic picks the
// wrong one easily (a wireless mouse dongle advertising a keyboard interface it
// never sends keys on). Missing the keyboard the user actually types on is fatal
// with session control, since the console is silenced and we are the only thing
// left that can switch VTs.
pub struct Keyboard {
    fds: Vec<i32>,
    keys: Vec<bool>,
    // Press/release edges this frame: (evdev keycode, pressed). Repeat is skipped.
    events: Vec<(u16, bool)>,
}

//
// Open
//

pub fn open() -> Option<Keyboard> {
    let mut fds: Vec<i32> = Vec::new();
    for (path, name) in find_keyboards() {
        let Ok(c) = CString::new(path.clone()) else { continue };
        let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_NONBLOCK) };
        if fd < 0 {
            eprintln!("om_wm: keyboard open failed: {path}");
            continue;
        }
        println!("om_wm: keyboard {path} ({name})");
        fds.push(fd);
    }
    if fds.is_empty() {
        eprintln!("om_wm: no keyboard device could be opened");
        return None;
    }
    // Opened ungrabbed: whether we need an exclusive grab depends on whether we
    // get session control, which main decides next.
    Some(Keyboard { fds, keys: vec![false; KEY_ARRAY], events: Vec::new() })
}

// Devices that expose the "kbd" handler and a key bitmap with letters and space
// in it. The bitmap is what separates a keyboard from a Power Button, a Lid
// Switch or a PC Speaker, all of which also carry a kbd handler.
fn find_keyboards() -> Vec<(String, String)> {
    let mut found: Vec<(String, String)> = Vec::new();
    let Ok(text) = fs::read_to_string("/proc/bus/input/devices") else { return found };
    for block in text.split("\n\n") {
        let mut name = String::new();
        let mut has_kbd_handler = false;
        let mut event: Option<String> = None;
        let mut keys: Vec<u64> = Vec::new();
        for line in block.lines() {
            if let Some(n) = line.strip_prefix("N: Name=") {
                name = n.trim_matches('"').to_string();
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
            if let Some(bits) = line.strip_prefix("B: KEY=") {
                keys = bitmap(bits);
            }
        }
        let types = bit_set(&keys, KEY_A) && bit_set(&keys, KEY_SPACE);
        if has_kbd_handler && types {
            if let Some(n) = event {
                found.push((format!("/dev/input/event{n}"), name));
            }
        }
    }
    found
}

// An evdev bitmap line from /proc/bus/input/devices: hex words of 64 bits, most
// significant word first. Returned least significant word first, so word n holds
// bits n*64 .. n*64+63.
pub fn bitmap(line: &str) -> Vec<u64> {
    let mut words: Vec<u64> = line
        .split_whitespace()
        .filter_map(|w| u64::from_str_radix(w, 16).ok())
        .collect();
    words.reverse();
    words
}

pub fn bit_set(words: &[u64], bit: u16) -> bool {
    let word = (bit / 64) as usize;
    let off = bit % 64;
    words.get(word).map(|w| (w >> off) & 1 == 1).unwrap_or(false)
}

//
// Poll / query
//

pub fn poll(kb: &mut Keyboard) {
    kb.events.clear();
    for i in 0..kb.fds.len() {
        read_device(kb.fds[i], &mut kb.keys, &mut kb.events);
    }
}

fn read_device(fd: i32, keys: &mut [bool], events: &mut Vec<(u16, bool)>) {
    let mut buf = [0u8; EVENT_SIZE];
    loop {
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut c_void, EVENT_SIZE) };
        if n != EVENT_SIZE as isize {
            break;
        }
        let etype = u16::from_ne_bytes([buf[16], buf[17]]);
        if etype != EV_KEY {
            continue;
        }
        let code = u16::from_ne_bytes([buf[18], buf[19]]);
        let value = i32::from_ne_bytes([buf[20], buf[21], buf[22], buf[23]]);
        if (code as usize) < keys.len() {
            // value 0 = up, 1 = down, 2 = repeat.
            keys[code as usize] = value != 0;
        }
        // Forward only real press/release edges (not autorepeat).
        if value == 0 || value == 1 {
            events.push((code, value == 1));
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

pub fn ctrl_down(kb: &Keyboard) -> bool {
    down(kb, KEY_LEFTCTRL) || down(kb, KEY_RIGHTCTRL)
}

pub fn alt_down(kb: &Keyboard) -> bool {
    down(kb, KEY_LEFTALT) || down(kb, KEY_RIGHTALT)
}

pub fn keys(kb: &Keyboard) -> &[bool] {
    &kb.keys
}

//
// Grab
//

// Take or release the exclusive grab. Only the no-session fallback needs it: with
// a session, logind puts our VT in K_OFF and the console ignores keys by itself,
// which also keeps its modifier state honest (a grab hides releases from it, and
// a stuck Alt there turns every later arrow key into a VT switch).
pub fn set_grab(kb: &mut Keyboard, on: bool) {
    let arg: libc::c_int = if on { 1 } else { 0 };
    for i in 0..kb.fds.len() {
        unsafe { libc::ioctl(kb.fds[i], EVIOCGRAB, arg) };
    }
}

// Throw away queued events and all key state. Keys pressed while another VT had
// the display were typed at that VT, not at us.
pub fn reset(kb: &mut Keyboard) {
    poll(kb);
    kb.events.clear();
    for k in kb.keys.iter_mut() {
        *k = false;
    }
}
