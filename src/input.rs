//
// Input (Data Oriented zone)
//
// libinput owns device discovery, hotplug and event decoding for keyboards and
// pointers. It is strictly better than reading evdev ourselves there: it finds
// every device through udev instead of a name heuristic, notices hotplug, and
// hands us buttons, horizontal scroll and high resolution wheels that our own
// decoder never read.
//
// The trackpad is the exception, and the only reason our own input code exists.
// libinput deliberately hides touchpad slots behind pointer motion, scroll and
// discrete pinch/swipe gestures, so the canvas feel (continuous simultaneous pan
// and zoom, with hysteresis and momentum) cannot be rebuilt from it faithfully.
// TRACKPAD_MODE picks which side drives it. In Custom mode we mute the trackpad
// inside libinput with config_send_events_set_mode(DISABLED) and read the raw
// device ourselves in touch.rs, which works because we no longer grab anything:
// libinput holds the node open, we read the same node, and only libinput's
// processing of it is switched off.
//
// All libinput FFI lives behind the `input` crate. The only raw work here is the
// device open path, which the crate hands us as a trait to implement.
//

use std::ffi::CString;
use std::fs;
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use input::event::device::DeviceEvent;
use input::event::gesture::{
    GestureEventCoordinates, GesturePinchEvent, GesturePinchEventTrait, GestureSwipeEvent,
};
use input::event::keyboard::{KeyState, KeyboardEvent, KeyboardEventTrait};
use input::event::pointer::{
    Axis, ButtonState, PointerEvent, PointerScrollEvent,
};
use input::event::{EventTrait, GestureEvent};
use input::{Device, DeviceCapability, Event, Libinput, LibinputInterface, SendEventsMode};

//
// Settings
//

// Which code drives the trackpad. Custom is our raw evdev gesture path
// (touch.rs); Libinput takes its pointer, scroll and pinch events instead.
// Runtime switching later only has to write Input::mode.
const TRACKPAD_MODE: TrackpadMode = TrackpadMode::Custom;

//
// Constants
//

const KEY_ARRAY: usize = 768;
// EVIOCGRAB = _IOW('E', 0x90, int): grab a device exclusively.
const EVIOCGRAB: libc::c_ulong = 0x4004_4590;
// Wheel events are reported in 1/120 of a notch.
const V120_PER_NOTCH: f32 = 120.0;

// Linux evdev codes (input-event-codes.h) we care about.
pub const KEY_ESC: u16 = 1;
pub const KEY_0: u16 = 11;
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

const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;

//
// Types
//

// A settings value: the arm that is not selected by TRACKPAD_MODE is unused by
// construction, which is not dead code.
#[allow(dead_code)]
#[derive(Clone, Copy, PartialEq)]
pub enum TrackpadMode {
    Custom,
    Libinput,
}

// Everything the pointer did since the last poll. Deltas are accumulated, button
// fields are edges except middle, which is a level for drag-to-pan.
#[derive(Clone, Copy, Default)]
pub struct Pointer {
    pub dx: f32,
    pub dy: f32,
    // Scroll sign convention, for every field below: positive is up and right,
    // which is evdev's (REL_WHEEL) and what INVERT_SCROLL is written against.
    // libinput reports positive as down and right, so the vertical axes are
    // flipped on the way in.
    //
    // Real wheels, in notches (fractional on high resolution mice).
    pub wheel: f32,
    pub hwheel: f32,
    // Finger and continuous scroll, in pixels. Only produced by the trackpad, so
    // only ever non-zero in Libinput trackpad mode.
    pub scroll_x: f32,
    pub scroll_y: f32,
    // Pinch factor for this frame (1.0 = no change), Libinput trackpad mode only.
    pub pinch: f32,
    pub left_pressed: bool,
    pub left_released: bool,
    pub right_pressed: bool,
    pub right_released: bool,
    // Middle is both: an edge for the double-click chord, a level for drag-to-pan.
    pub middle_pressed: bool,
    pub middle: bool,
}

pub struct Input {
    li: Libinput,
    mode: TrackpadMode,
    keys: Vec<bool>,
    // Press/release edges this frame: (evdev keycode, pressed).
    events: Vec<(u16, bool)>,
    pointer: Pointer,
    // Middle button level, kept across frames.
    middle: bool,
    // Absolute pinch scale reported at the last gesture update, to turn
    // libinput's since-gesture-began scale into a per frame factor.
    pinch_scale: f32,
    // Sub-pixel motion carried into the next frame. libinput's deltas are
    // accelerated and fractional; truncating them every frame would swallow slow
    // movement entirely.
    frac_x: f32,
    frac_y: f32,
    keyboards: u32,
    pointers: u32,
    // Device node of the trackpad we muted for touch.rs, as libinput reports it.
    // None when there is none, or when the trackpad is libinput's to drive.
    trackpad: Option<String>,
    // Set whenever that trackpad appeared or went away, so the raw reader is
    // reopened rather than left on a stale fd. An unplug and replug can land on
    // the same node, so the node alone is not enough to notice.
    trackpad_changed: bool,
}

// libinput opens device nodes through us.
struct Interface {
    // Without session control the console still reads the keyboard, and an
    // exclusive grab as the device is opened is the only lever we have to keep
    // what we type out of it. With a session, logind's K_OFF does that instead.
    //
    // Keyboards only. Grabbing a pointer would buy nothing (the console ignores
    // them) and would starve our own raw reader of the trackpad, since a grab is
    // exclusive against every other handle on the device, including ours.
    grab_keyboards: bool,
}

impl LibinputInterface for Interface {
    fn open_restricted(&mut self, path: &Path, flags: i32) -> Result<OwnedFd, i32> {
        let Ok(c) = CString::new(path.as_os_str().as_bytes()) else {
            return Err(libc::EINVAL);
        };
        let fd = unsafe { libc::open(c.as_ptr(), flags) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(libc::EACCES));
        }
        if self.grab_keyboards && fd_types(fd) {
            unsafe { libc::ioctl(fd, EVIOCGRAB, 1 as libc::c_int) };
        }
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    fn close_restricted(&mut self, fd: OwnedFd) {
        drop(fd);
    }
}

// Whether an open device can type: letters and space in its EV_KEY bitmap, the
// same test as any_keyboard_present but straight from the fd.
// EVIOCGBIT(EV_KEY, len) = _IOR('E', 0x20 + EV_KEY, len).
fn fd_types(fd: i32) -> bool {
    const KEY_BYTES: usize = KEY_ARRAY / 8;
    let req: libc::c_ulong =
        (2 << 30) | ((KEY_BYTES as libc::c_ulong) << 16) | (0x45 << 8) | 0x21;
    let mut bits = [0u8; KEY_BYTES];
    if unsafe { libc::ioctl(fd, req, bits.as_mut_ptr()) } < 0 {
        return false;
    }
    byte_bit(&bits, KEY_A) && byte_bit(&bits, KEY_SPACE)
}

fn byte_bit(bits: &[u8], bit: u16) -> bool {
    let byte = (bit / 8) as usize;
    let off = bit % 8;
    bits.get(byte).map(|b| (b >> off) & 1 == 1).unwrap_or(false)
}

//
// Open
//

pub fn init(grab: bool) -> Option<Input> {
    let seat = std::env::var("XDG_SEAT").unwrap_or_else(|_| "seat0".to_string());
    let mut li = Libinput::new_with_udev(Interface { grab_keyboards: grab });
    if li.udev_assign_seat(&seat).is_err() {
        eprintln!("om_wm: input: libinput could not assign seat {seat}");
        return None;
    }
    let mut inp = Input {
        li,
        mode: TRACKPAD_MODE,
        keys: vec![false; KEY_ARRAY],
        events: Vec::new(),
        pointer: Pointer::default(),
        middle: false,
        pinch_scale: 1.0,
        frac_x: 0.0,
        frac_y: 0.0,
        keyboards: 0,
        pointers: 0,
        trackpad: None,
        trackpad_changed: false,
    };
    // Drain the initial device list, so devices are counted and the trackpad is
    // muted before the first frame rather than after it.
    poll(&mut inp);
    reset(&mut inp);
    if inp.keyboards == 0 {
        eprintln!("om_wm: input: libinput found no keyboard");
    }
    // The keyboard count is libinput's keyboard capability, which power and lid
    // buttons carry too, so it is a device count and not a count of things you can
    // type on.
    println!(
        "om_wm: libinput on {seat}: {} key device(s), {} pointer(s), trackpad {}{}",
        inp.keyboards,
        inp.pointers,
        if inp.mode == TrackpadMode::Custom { "custom" } else { "libinput" },
        if grab { ", devices grabbed" } else { "" }
    );
    Some(inp)
}

// Whether the machine has a keyboard at all, read from /proc so it can be
// answered before libinput (and before session control, which must not silence
// the console when we have no keyboard to switch VTs with). A keyboard is a
// device with the kbd handler whose key bitmap has letters and space in it, which
// is what separates it from a Power Button, Lid Switch or PC Speaker.
pub fn any_keyboard_present() -> bool {
    let Ok(text) = fs::read_to_string("/proc/bus/input/devices") else { return false };
    for block in text.split("\n\n") {
        let mut has_kbd_handler = false;
        let mut keys: Vec<u64> = Vec::new();
        for line in block.lines() {
            if let Some(handlers) = line.strip_prefix("H: Handlers=") {
                has_kbd_handler = handlers.split_whitespace().any(|t| t == "kbd");
            }
            if let Some(bits) = line.strip_prefix("B: KEY=") {
                keys = bitmap(bits);
            }
        }
        if has_kbd_handler && bit_set(&keys, KEY_A) && bit_set(&keys, KEY_SPACE) {
            return true;
        }
    }
    false
}

// An evdev bitmap line from /proc/bus/input/devices: hex words of 64 bits, most
// significant word first. Returned least significant word first, so word n holds
// bits n*64 .. n*64+63.
fn bitmap(line: &str) -> Vec<u64> {
    let mut words: Vec<u64> = line
        .split_whitespace()
        .filter_map(|w| u64::from_str_radix(w, 16).ok())
        .collect();
    words.reverse();
    words
}

fn bit_set(words: &[u64], bit: u16) -> bool {
    let word = (bit / 64) as usize;
    let off = bit % 64;
    words.get(word).map(|w| (w >> off) & 1 == 1).unwrap_or(false)
}

//
// Poll
//

pub fn poll(inp: &mut Input) {
    inp.events.clear();
    let mut p = Pointer { pinch: 1.0, ..Pointer::default() };
    if let Err(e) = inp.li.dispatch() {
        eprintln!("om_wm: input: libinput dispatch failed: {e}");
    }
    while let Some(event) = inp.li.next() {
        match event {
            Event::Device(DeviceEvent::Added(e)) => added(inp, &e.device()),
            Event::Device(DeviceEvent::Removed(e)) => removed(inp, &e.device()),
            Event::Keyboard(KeyboardEvent::Key(e)) => {
                let code = e.key() as u16;
                let pressed = e.key_state() == KeyState::Pressed;
                if (code as usize) < inp.keys.len() {
                    inp.keys[code as usize] = pressed;
                }
                inp.events.push((code, pressed));
            }
            Event::Pointer(pe) => pointer_event(inp, &mut p, pe),
            Event::Gesture(ge) => gesture_event(inp, &mut p, ge),
            _ => {}
        }
    }
    // Whole pixels this frame, remainder kept for the next one.
    let fx = inp.frac_x + p.dx;
    let fy = inp.frac_y + p.dy;
    p.dx = fx.trunc();
    p.dy = fy.trunc();
    inp.frac_x = fx - p.dx;
    inp.frac_y = fy - p.dy;

    p.middle = inp.middle;
    inp.pointer = p;
}

fn pointer_event(inp: &mut Input, p: &mut Pointer, event: PointerEvent) {
    match event {
        PointerEvent::Motion(e) => {
            p.dx += e.dx() as f32;
            p.dy += e.dy() as f32;
        }
        PointerEvent::Button(e) => {
            let down = e.button_state() == ButtonState::Pressed;
            match e.button() {
                BTN_LEFT => {
                    if down {
                        p.left_pressed = true;
                    } else {
                        p.left_released = true;
                    }
                }
                BTN_RIGHT => {
                    if down {
                        p.right_pressed = true;
                    } else {
                        p.right_released = true;
                    }
                }
                BTN_MIDDLE => {
                    inp.middle = down;
                    if down {
                        p.middle_pressed = true;
                    }
                }
                _ => {}
            }
        }
        // Vertical axes are negated here: libinput counts down as positive, we
        // count up as positive (see Pointer).
        // Each axis has to be checked before it is read: asking for an axis the
        // event does not carry is a client bug and libinput says so, loudly.
        PointerEvent::ScrollWheel(e) => {
            if e.has_axis(Axis::Vertical) {
                p.wheel -= e.scroll_value_v120(Axis::Vertical) as f32 / V120_PER_NOTCH;
            }
            if e.has_axis(Axis::Horizontal) {
                p.hwheel += e.scroll_value_v120(Axis::Horizontal) as f32 / V120_PER_NOTCH;
            }
        }
        PointerEvent::ScrollFinger(e) => {
            if e.has_axis(Axis::Horizontal) {
                p.scroll_x += e.scroll_value(Axis::Horizontal) as f32;
            }
            if e.has_axis(Axis::Vertical) {
                p.scroll_y -= e.scroll_value(Axis::Vertical) as f32;
            }
        }
        PointerEvent::ScrollContinuous(e) => {
            if e.has_axis(Axis::Horizontal) {
                p.scroll_x += e.scroll_value(Axis::Horizontal) as f32;
            }
            if e.has_axis(Axis::Vertical) {
                p.scroll_y -= e.scroll_value(Axis::Vertical) as f32;
            }
        }
        // Absolute pointers (tablets, touchscreens) are not mapped yet.
        _ => {}
    }
}

// Only reachable in Libinput trackpad mode: in Custom mode the trackpad is muted
// inside libinput, and it is the only device here that produces gestures.
fn gesture_event(inp: &mut Input, p: &mut Pointer, event: GestureEvent) {
    match event {
        GestureEvent::Pinch(GesturePinchEvent::Begin(_)) => inp.pinch_scale = 1.0,
        GestureEvent::Pinch(GesturePinchEvent::Update(e)) => {
            // libinput reports scale relative to the start of the gesture; the
            // canvas wants a per frame factor.
            let scale = e.scale() as f32;
            if inp.pinch_scale > 0.0 && scale > 0.0 {
                p.pinch *= scale / inp.pinch_scale;
            }
            inp.pinch_scale = scale;
            p.scroll_x += e.dx() as f32;
            p.scroll_y -= e.dy() as f32;
        }
        GestureEvent::Pinch(GesturePinchEvent::End(_)) => inp.pinch_scale = 1.0,
        GestureEvent::Swipe(GestureSwipeEvent::Update(e)) => {
            p.scroll_x += e.dx() as f32;
            p.scroll_y -= e.dy() as f32;
        }
        _ => {}
    }
}

//
// Devices
//

fn added(inp: &mut Input, dev: &Device) {
    if dev.has_capability(DeviceCapability::Keyboard) {
        inp.keyboards += 1;
    }
    if dev.has_capability(DeviceCapability::Pointer) {
        inp.pointers += 1;
    }
    let pad = is_touchpad(dev);
    if pad && inp.mode == TrackpadMode::Custom {
        // Mute it here; touch.rs reads the same device raw for gestures. libinput
        // found it and names its node, so nothing has to guess at either.
        if dev.config_send_events_set_mode(SendEventsMode::DISABLED).is_err() {
            eprintln!(
                "om_wm: input: could not mute {} in libinput, it will fight touch.rs",
                dev.name()
            );
        } else {
            let node = node_of(dev);
            println!("om_wm: input + {} (trackpad {node}, muted for touch.rs)", dev.name());
            inp.trackpad = Some(node);
            inp.trackpad_changed = true;
            return;
        }
    }
    println!("om_wm: input + {}{}", dev.name(), if pad { " (trackpad)" } else { "" });
}

fn removed(inp: &mut Input, dev: &Device) {
    if inp.trackpad.as_deref() == Some(node_of(dev).as_str()) {
        inp.trackpad = None;
        inp.trackpad_changed = true;
    }
    if dev.has_capability(DeviceCapability::Keyboard) {
        inp.keyboards = inp.keyboards.saturating_sub(1);
    }
    if dev.has_capability(DeviceCapability::Pointer) {
        inp.pointers = inp.pointers.saturating_sub(1);
    }
    println!("om_wm: input - {}", dev.name());
}

// Where the device lives, from libinput's sysname (e.g. "event8").
fn node_of(dev: &Device) -> String {
    format!("/dev/input/{}", dev.sysname())
}

// libinput's own test for a touchpad: a pointer that reports a tap finger count.
fn is_touchpad(dev: &Device) -> bool {
    dev.has_capability(DeviceCapability::Pointer) && dev.config_tap_finger_count() > 0
}

//
// Query
//

pub fn pointer(inp: &Input) -> Pointer {
    inp.pointer
}

pub fn down(inp: &Input, code: u16) -> bool {
    inp.keys.get(code as usize).copied().unwrap_or(false)
}

pub fn events(inp: &Input) -> &[(u16, bool)] {
    &inp.events
}

pub fn keys(inp: &Input) -> &[bool] {
    &inp.keys
}

pub fn super_down(inp: &Input) -> bool {
    down(inp, KEY_LEFTMETA) || down(inp, KEY_RIGHTMETA)
}

pub fn ctrl_down(inp: &Input) -> bool {
    down(inp, KEY_LEFTCTRL) || down(inp, KEY_RIGHTCTRL)
}

pub fn alt_down(inp: &Input) -> bool {
    down(inp, KEY_LEFTALT) || down(inp, KEY_RIGHTALT)
}

// The trackpad node touch.rs should be reading, if any.
pub fn trackpad_node(inp: &Input) -> Option<&str> {
    inp.trackpad.as_deref()
}

// Whether that trackpad came or went since this was last asked. Read and clear,
// so the caller reopens exactly once per change.
pub fn trackpad_changed(inp: &mut Input) -> bool {
    let changed = inp.trackpad_changed;
    inp.trackpad_changed = false;
    changed
}

//
// Session handover
//

// Hand the devices back while another VT owns the display, and take them again
// when it returns. libinput closes and reopens the nodes for us.
pub fn suspend(inp: &mut Input) {
    inp.li.suspend();
}

pub fn resume(inp: &mut Input) {
    if inp.li.resume().is_err() {
        eprintln!("om_wm: input: libinput resume failed");
    }
    reset(inp);
}

// Forget everything: input that arrived while another VT had the display was not
// meant for us.
pub fn reset(inp: &mut Input) {
    inp.events.clear();
    inp.pointer = Pointer::default();
    inp.middle = false;
    inp.pinch_scale = 1.0;
    inp.frac_x = 0.0;
    inp.frac_y = 0.0;
    for k in inp.keys.iter_mut() {
        *k = false;
    }
}
