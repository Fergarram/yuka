//
// Touchpad (Data Oriented zone)
//
// Manual multitouch gesture handling for the trackpad. We read the raw evdev
// multitouch stream (protocol B) directly, track each finger by slot, and each
// frame derive two gestures at once: two-finger pan (centroid movement) and
// pinch zoom (finger-distance ratio). No libinput, no scroll-vs-pinch mode
// switching, so pan and zoom happen together and continuously.
//
// All the raw fd/read work is contained here; callers get camera updates.
//

use std::ffi::{c_void, CString};
use std::fs;

use crate::camera::{self, Camera};
use crate::cursor::{self, Cursor};
use crate::ray;

//
// Constants (evdev)
//

const EV_ABS: u16 = 0x03;
const EV_KEY: u16 = 0x01;
const ABS_MT_SLOT: u16 = 0x2F;
const ABS_MT_POSITION_X: u16 = 0x35;
const ABS_MT_POSITION_Y: u16 = 0x36;
const ABS_MT_TRACKING_ID: u16 = 0x39;
const BTN_LEFT: u16 = 0x110;

// struct input_event is 24 bytes on 64-bit Linux: 16-byte timeval, u16 type,
// u16 code, i32 value.
const EVENT_SIZE: usize = 24;
// EVIOCGRAB = _IOW('E', 0x90, int): grab a device exclusively.
const EVIOCGRAB: libc::c_ulong = 0x4004_4590;

const MAX_SLOTS: usize = 16;

// Feel (these are the natural home for future config settings):
// canvas pixels panned per trackpad device unit at zoom 1.0. Divided by zoom so
// panning feels the same at any scale.
const PAN_SENS: f32 = 0.12;
// A two-finger gesture is pan OR zoom at any instant, never both. The active
// mode is sticky and switches only when the other motion dominates by this
// factor (hysteresis), so pan and zoom stay clean but interchange fluidly
// without lifting.
const MODE_BIAS: f32 = 1.6;
// Minimum per-frame motion (device units) before a mode is chosen at all.
const MODE_EPS: f32 = 1.5;
// While zooming, ignore per-frame distance changes below this (device units).
const PINCH_DEADZONE: f32 = 2.0;
// Exponential smoothing of the tracked pan velocity (0..1, higher = snappier).
const PAN_SMOOTH: f32 = 0.4;
// Momentum: how fast the release glide decays (per second) and when to stop.
const PAN_FRICTION: f32 = 2.0;
const PAN_STOP_SPEED: f32 = 5.0;
// One-finger pointer motion: screen pixels moved per trackpad device unit.
const POINTER_SENS: f32 = 0.25;

//
// Types
//

#[derive(Clone, Copy)]
struct Slot {
    active: bool,
    x: i32,
    y: i32,
}

// Which transform a two-finger gesture is currently applying.
#[derive(Clone, Copy, PartialEq)]
enum GestureMode {
    None,
    Pan,
    Zoom,
}

pub struct Touchpad {
    fd: i32,
    slots: [Slot; MAX_SLOTS],
    cur: usize,
    // Baselines for the current two-finger gesture; None when not exactly two
    // fingers are down, so a new gesture never jumps.
    prev_centroid: Option<(f32, f32)>,
    prev_dist: Option<f32>,
    // Which of pan/zoom the gesture is currently applying.
    mode: GestureMode,
    // Smoothed pan velocity (canvas units/sec) for release momentum.
    pan_vx: f32,
    pan_vy: f32,
    // Pointer tracking: which slot drives the cursor, its previous pos, and the
    // fractional remainder for slow motion.
    primary_slot: Option<usize>,
    prev_single: Option<(f32, f32)>,
    ptr_accum_x: f32,
    ptr_accum_y: f32,
    // Physical trackpad button (BTN_LEFT): current level + per-frame edges.
    btn_left: bool,
    click_pressed: bool,
    click_released: bool,
}

//
// Open
//

pub fn open() -> Option<Touchpad> {
    let path = find_touchpad()?;
    let c = CString::new(path.clone()).ok()?;
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_NONBLOCK) };
    if fd < 0 {
        eprintln!("om_wm: touchpad open failed: {path}");
        return None;
    }
    unsafe { libc::ioctl(fd, EVIOCGRAB, 1 as libc::c_int) };
    println!("om_wm: touchpad {path}");
    Some(Touchpad {
        fd,
        slots: [Slot { active: false, x: 0, y: 0 }; MAX_SLOTS],
        cur: 0,
        prev_centroid: None,
        prev_dist: None,
        mode: GestureMode::None,
        pan_vx: 0.0,
        pan_vy: 0.0,
        primary_slot: None,
        prev_single: None,
        ptr_accum_x: 0.0,
        ptr_accum_y: 0.0,
        btn_left: false,
        click_pressed: false,
        click_released: false,
    })
}

// Find the trackpad's event node via /proc/bus/input/devices (a multitouch
// device by name). Returns e.g. "/dev/input/event4".
fn find_touchpad() -> Option<String> {
    let text = fs::read_to_string("/proc/bus/input/devices").ok()?;
    for block in text.split("\n\n") {
        let mut is_pad = false;
        let mut event: Option<String> = None;
        for line in block.lines() {
            if let Some(name) = line.strip_prefix("N: Name=") {
                let low = name.to_lowercase();
                is_pad = low.contains("bcm5974")
                    || low.contains("trackpad")
                    || low.contains("touchpad");
            }
            if let Some(handlers) = line.strip_prefix("H: Handlers=") {
                for tok in handlers.split_whitespace() {
                    if let Some(n) = tok.strip_prefix("event") {
                        event = Some(n.to_string());
                    }
                }
            }
        }
        if is_pad {
            if let Some(n) = event {
                return Some(format!("/dev/input/event{n}"));
            }
        }
    }
    None
}

//
// Grab
//

// Hold or release the exclusive grab (dropped while another VT owns the display).
pub fn set_grab(tp: &mut Touchpad, on: bool) {
    let arg: libc::c_int = if on { 1 } else { 0 };
    unsafe { libc::ioctl(tp.fd, EVIOCGRAB, arg) };
}

// Drain queued events and forget all finger/gesture state, so returning from
// another VT does not replay a stale gesture into the camera.
pub fn reset(tp: &mut Touchpad) {
    read_events(tp);
    tp.slots = [Slot { active: false, x: 0, y: 0 }; MAX_SLOTS];
    tp.cur = 0;
    tp.prev_centroid = None;
    tp.prev_dist = None;
    tp.mode = GestureMode::None;
    tp.pan_vx = 0.0;
    tp.pan_vy = 0.0;
    tp.primary_slot = None;
    tp.prev_single = None;
    tp.ptr_accum_x = 0.0;
    tp.ptr_accum_y = 0.0;
    tp.btn_left = false;
    tp.click_pressed = false;
    tp.click_released = false;
}

//
// Update
//

fn read_events(tp: &mut Touchpad) {
    let mut buf = [0u8; EVENT_SIZE];
    loop {
        let n = unsafe {
            libc::read(tp.fd, buf.as_mut_ptr() as *mut c_void, EVENT_SIZE)
        };
        if n != EVENT_SIZE as isize {
            break; // EAGAIN (-1), EOF (0), or short read
        }

        let etype = u16::from_ne_bytes([buf[16], buf[17]]);
        let code = u16::from_ne_bytes([buf[18], buf[19]]);
        let value = i32::from_ne_bytes([buf[20], buf[21], buf[22], buf[23]]);

        match (etype, code) {
            (EV_ABS, ABS_MT_SLOT) => {
                let s = value as usize;
                if s < MAX_SLOTS {
                    tp.cur = s;
                }
            }
            (EV_ABS, ABS_MT_TRACKING_ID) => {
                if tp.cur < MAX_SLOTS {
                    tp.slots[tp.cur].active = value != -1;
                }
            }
            (EV_ABS, ABS_MT_POSITION_X) => {
                if tp.cur < MAX_SLOTS {
                    tp.slots[tp.cur].x = value;
                }
            }
            (EV_ABS, ABS_MT_POSITION_Y) => {
                if tp.cur < MAX_SLOTS {
                    tp.slots[tp.cur].y = value;
                }
            }
            (EV_KEY, BTN_LEFT) => {
                if value == 1 {
                    tp.btn_left = true;
                    tp.click_pressed = true;
                } else if value == 0 {
                    tp.btn_left = false;
                    tp.click_released = true;
                }
            }
            _ => {}
        }
    }
}

// Drain trackpad events and apply gestures: one finger moves the pointer, two
// fingers pan/zoom (only when gestures_enabled, i.e. no window focused).
// Returns the BTN_LEFT (pressed, released) edges this frame for the caller to
// route as a click.
pub fn update(
    tp: &mut Touchpad,
    cam: &mut Camera,
    cursor: Option<&mut Cursor>,
    gestures_enabled: bool,
) -> (bool, bool) {
    tp.click_pressed = false;
    tp.click_released = false;
    read_events(tp);
    let clicks = (tp.click_pressed, tp.click_released);
    let dt = ray::frame_time().max(1e-4);

    // Cursor position is the pinch-zoom origin (screen space).
    let screen_w = ray::screen_width() as f32;
    let screen_h = ray::screen_height() as f32;
    let zoom_origin = cursor.as_deref().map(cursor::pos);

    let mut pts: [(f32, f32); 2] = [(0.0, 0.0); 2];
    let mut count = 0usize;
    for s in &tp.slots {
        if s.active {
            if count < 2 {
                pts[count] = (s.x as f32, s.y as f32);
            }
            count += 1;
        }
    }

    // Pointer motion. Unfocused: only a single finger drives the cursor (two
    // fingers are gestures). Focused: the primary (first) finger always drives
    // the cursor and extra fingers do nothing. The primary is tracked by slot so
    // lifting a non-primary finger never jumps.
    let do_pointer = if gestures_enabled { count == 1 } else { count >= 1 };
    if do_pointer {
        // Latch onto the original finger's slot and keep it until that finger
        // lifts; extra fingers are ignored. Adopt a new primary only once the
        // current one is up.
        let primary = match tp.primary_slot {
            Some(s) if tp.slots[s].active => Some(s),
            _ => tp.slots.iter().position(|s| s.active),
        };
        if primary != tp.primary_slot {
            tp.primary_slot = primary;
            tp.prev_single = None;
        }
        if let Some(p) = primary {
            let (fx, fy) = (tp.slots[p].x as f32, tp.slots[p].y as f32);
            if let (Some((px, py)), Some(cur)) = (tp.prev_single, cursor) {
                tp.ptr_accum_x += (fx - px) * POINTER_SENS;
                tp.ptr_accum_y += (fy - py) * POINTER_SENS;
                let idx = tp.ptr_accum_x.trunc();
                let idy = tp.ptr_accum_y.trunc();
                tp.ptr_accum_x -= idx;
                tp.ptr_accum_y -= idy;
                cursor::move_by(cur, idx as i32, idy as i32);
            }
            tp.prev_single = Some((fx, fy));
        }
        tp.prev_centroid = None;
        tp.prev_dist = None;
        tp.mode = GestureMode::None;
        return clicks;
    }
    tp.prev_single = None;
    tp.primary_slot = None;

    // Not a two-finger gesture: coast any leftover pan momentum with friction.
    if count != 2 {
        tp.prev_centroid = None;
        tp.prev_dist = None;
        tp.mode = GestureMode::None;
        if tp.pan_vx != 0.0 || tp.pan_vy != 0.0 {
            cam.cx += tp.pan_vx * dt;
            cam.cy += tp.pan_vy * dt;
            let decay = (-PAN_FRICTION * dt).exp();
            tp.pan_vx *= decay;
            tp.pan_vy *= decay;
            if (tp.pan_vx * tp.pan_vx + tp.pan_vy * tp.pan_vy).sqrt()
                < PAN_STOP_SPEED
            {
                tp.pan_vx = 0.0;
                tp.pan_vy = 0.0;
            }
        }
        return clicks;
    }

    let centroid = ((pts[0].0 + pts[1].0) * 0.5, (pts[0].1 + pts[1].1) * 0.5);
    let dx = pts[0].0 - pts[1].0;
    let dy = pts[0].1 - pts[1].1;
    let dist = (dx * dx + dy * dy).sqrt();

    // Focused window: pan/zoom disabled. Keep baselines fresh so re-enabling
    // (after unfocus) does not jump.
    if !gestures_enabled {
        tp.prev_centroid = Some(centroid);
        tp.prev_dist = Some(dist);
        tp.mode = GestureMode::None;
        return clicks;
    }

    match (tp.prev_centroid, tp.prev_dist) {
        (Some(pc), Some(pd)) => {
            let pan_delta =
                ((centroid.0 - pc.0).powi(2) + (centroid.1 - pc.1).powi(2)).sqrt();
            let zoom_delta = (dist - pd).abs();

            // Pick the dominant intent, with hysteresis: only switch out of the
            // current mode when the other motion clearly leads.
            let want_zoom =
                zoom_delta > MODE_EPS && zoom_delta > pan_delta * MODE_BIAS;
            let want_pan =
                pan_delta > MODE_EPS && pan_delta > zoom_delta * MODE_BIAS;
            tp.mode = match tp.mode {
                GestureMode::Pan => {
                    if want_zoom { GestureMode::Zoom } else { GestureMode::Pan }
                }
                GestureMode::Zoom => {
                    if want_pan { GestureMode::Pan } else { GestureMode::Zoom }
                }
                GestureMode::None => {
                    if want_zoom {
                        GestureMode::Zoom
                    } else if want_pan {
                        GestureMode::Pan
                    } else {
                        GestureMode::None
                    }
                }
            };

            match tp.mode {
                GestureMode::Pan => {
                    // Content follows the fingers, so the view shifts opposite.
                    let move_x = -(centroid.0 - pc.0) * PAN_SENS / cam.zoom;
                    let move_y = -(centroid.1 - pc.1) * PAN_SENS / cam.zoom;
                    cam.cx += move_x;
                    cam.cy += move_y;
                    let alpha = PAN_SMOOTH;
                    tp.pan_vx = tp.pan_vx * (1.0 - alpha) + (move_x / dt) * alpha;
                    tp.pan_vy = tp.pan_vy * (1.0 - alpha) + (move_y / dt) * alpha;
                }
                GestureMode::Zoom => {
                    // No pan; drop momentum so lifting after a zoom does not glide.
                    tp.pan_vx = 0.0;
                    tp.pan_vy = 0.0;
                    let ddist = dist - pd;
                    if ddist.abs() > PINCH_DEADZONE {
                        let beyond = ddist - ddist.signum() * PINCH_DEADZONE;
                        let ratio = (pd + beyond) / pd;
                        let (ox, oy) = zoom_origin
                            .map(|(x, y)| (x as f32, y as f32))
                            .unwrap_or((screen_w * 0.5, screen_h * 0.5));
                        camera::zoom_at(cam, ratio, ox, oy, screen_w, screen_h);
                    }
                }
                GestureMode::None => {}
            }
        }
        _ => {
            // Gesture start: grabbing cancels momentum.
            tp.pan_vx = 0.0;
            tp.pan_vy = 0.0;
            tp.mode = GestureMode::None;
        }
    }

    tp.prev_centroid = Some(centroid);
    tp.prev_dist = Some(dist);
    clicks
}
