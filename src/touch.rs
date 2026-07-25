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

use crate::camera::Camera;
use crate::ray;

//
// Constants (evdev)
//

const EV_ABS: u16 = 0x03;
const ABS_MT_SLOT: u16 = 0x2F;
const ABS_MT_POSITION_X: u16 = 0x35;
const ABS_MT_POSITION_Y: u16 = 0x36;
const ABS_MT_TRACKING_ID: u16 = 0x39;

// struct input_event is 24 bytes on 64-bit Linux: 16-byte timeval, u16 type,
// u16 code, i32 value.
const EVENT_SIZE: usize = 24;

const MAX_SLOTS: usize = 16;

// Feel (these are the natural home for future config settings):
// canvas pixels panned per trackpad device unit at zoom 1.0. Divided by zoom so
// panning feels the same at any scale.
const PAN_SENS: f32 = 0.12;
const ZOOM_MIN: f32 = 0.1;
const ZOOM_MAX: f32 = 8.0;
// Zoom only engages once the fingers have spread/closed by this much (device
// units) from the gesture start, so incidental jitter while panning never zooms;
// only a deliberate, sustained pinch does.
const PINCH_ENGAGE_DIST: f32 = 40.0;
// After engaging, ignore per-frame distance changes below this (device units) to
// kill residual jitter. Soft: only movement beyond it zooms.
const PINCH_DEADZONE: f32 = 2.0;
// Exponential smoothing of the tracked pan velocity (0..1, higher = snappier).
const PAN_SMOOTH: f32 = 0.4;
// Momentum: how fast the release glide decays (per second) and when to stop.
const PAN_FRICTION: f32 = 3.0;
const PAN_STOP_SPEED: f32 = 5.0;

//
// Types
//

#[derive(Clone, Copy)]
struct Slot {
    active: bool,
    x: i32,
    y: i32,
}

pub struct Touchpad {
    fd: i32,
    slots: [Slot; MAX_SLOTS],
    cur: usize,
    // Pan baseline for the current two-finger gesture; None when not exactly two
    // fingers are down, so a new gesture never jumps.
    prev_centroid: Option<(f32, f32)>,
    // Zoom gate: finger distance at gesture start, whether zoom has engaged, and
    // the running reference distance once engaged.
    dist_start: f32,
    zoom_engaged: bool,
    zoom_ref: f32,
    // Smoothed pan velocity (canvas units/sec) for release momentum.
    pan_vx: f32,
    pan_vy: f32,
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
    println!("om_wm: touchpad {path}");
    Some(Touchpad {
        fd,
        slots: [Slot { active: false, x: 0, y: 0 }; MAX_SLOTS],
        cur: 0,
        prev_centroid: None,
        dist_start: 0.0,
        zoom_engaged: false,
        zoom_ref: 0.0,
        pan_vx: 0.0,
        pan_vy: 0.0,
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
        if etype != EV_ABS {
            continue;
        }
        let code = u16::from_ne_bytes([buf[18], buf[19]]);
        let value = i32::from_ne_bytes([buf[20], buf[21], buf[22], buf[23]]);

        match code {
            ABS_MT_SLOT => {
                let s = value as usize;
                if s < MAX_SLOTS {
                    tp.cur = s;
                }
            }
            ABS_MT_TRACKING_ID => {
                if tp.cur < MAX_SLOTS {
                    tp.slots[tp.cur].active = value != -1;
                }
            }
            ABS_MT_POSITION_X => {
                if tp.cur < MAX_SLOTS {
                    tp.slots[tp.cur].x = value;
                }
            }
            ABS_MT_POSITION_Y => {
                if tp.cur < MAX_SLOTS {
                    tp.slots[tp.cur].y = value;
                }
            }
            _ => {}
        }
    }
}

// Drain trackpad events and apply two-finger pan + pinch zoom to the camera,
// with a pinch deadzone and release momentum for panning.
pub fn update(tp: &mut Touchpad, cam: &mut Camera) {
    read_events(tp);
    let dt = ray::frame_time().max(1e-4);

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

    // Not a two-finger gesture: coast any leftover pan momentum with friction.
    if count != 2 {
        tp.prev_centroid = None;
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
        return;
    }

    let centroid = ((pts[0].0 + pts[1].0) * 0.5, (pts[0].1 + pts[1].1) * 0.5);
    let dx = pts[0].0 - pts[1].0;
    let dy = pts[0].1 - pts[1].1;
    let dist = (dx * dx + dy * dy).sqrt();

    match tp.prev_centroid {
        Some(pc) => {
            // Pan: content follows the fingers, so the view shifts opposite.
            let move_x = -(centroid.0 - pc.0) * PAN_SENS / cam.zoom;
            let move_y = -(centroid.1 - pc.1) * PAN_SENS / cam.zoom;
            cam.cx += move_x;
            cam.cy += move_y;

            // Track a smoothed velocity for the release glide.
            let alpha = PAN_SMOOTH;
            tp.pan_vx = tp.pan_vx * (1.0 - alpha) + (move_x / dt) * alpha;
            tp.pan_vy = tp.pan_vy * (1.0 - alpha) + (move_y / dt) * alpha;

            // Zoom is gated: it stays off until the fingers have spread/closed by
            // PINCH_ENGAGE_DIST from the gesture start (a deliberate pinch), then
            // tracks the distance ratio with a small deadzone against jitter.
            if !tp.zoom_engaged {
                if (dist - tp.dist_start).abs() > PINCH_ENGAGE_DIST {
                    tp.zoom_engaged = true;
                    tp.zoom_ref = dist;
                }
            } else {
                let ddist = dist - tp.zoom_ref;
                if tp.zoom_ref > 1.0 && ddist.abs() > PINCH_DEADZONE {
                    let beyond = ddist - ddist.signum() * PINCH_DEADZONE;
                    let ratio = (tp.zoom_ref + beyond) / tp.zoom_ref;
                    cam.zoom = (cam.zoom * ratio).clamp(ZOOM_MIN, ZOOM_MAX);
                }
                tp.zoom_ref = dist;
            }
        }
        None => {
            // Gesture start: grabbing cancels momentum; arm the zoom gate.
            tp.pan_vx = 0.0;
            tp.pan_vy = 0.0;
            tp.dist_start = dist;
            tp.zoom_engaged = false;
        }
    }

    tp.prev_centroid = Some(centroid);
}
