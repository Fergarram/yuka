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

// Distance thresholds are fractions of the shorter axis of the device's
// coordinate span, since trackpads report no physical resolution and their units
// are meaningless on their own. This one spans 9760 x 6750 units over roughly
// 105 x 75 mm, so about 90 units per mm, and the comments below give the
// millimetres that works out to there. Absolute unit counts were the bug they
// replace: 12 units read as a generous tap wobble and is a tenth of a
// millimetre.
//
// How far the fingers must travel before a two-finger gesture pans at all. It
// doubles as the drift a contact may have and still count as a tap, so a contact
// either pans or stays eligible as a tap, never both.
const MOVE_START_FRAC: f32 = 0.012; // ~81 units, ~0.9 mm
// How much the finger distance must change, in total from where the gesture
// began, before a pinch zooms at all. Below this the gesture is a tap, a resting
// hand or a pan, none of which should nudge the zoom.
const PINCH_START_FRAC: f32 = 0.02; // ~135 units, ~1.5 mm
// Minimum per-frame motion before pan-versus-zoom dominance is judged.
const MODE_EPS_FRAC: f32 = 0.0015; // ~10 units
// Per-frame pinch jitter ignored once armed.
const PINCH_DEADZONE_FRAC: f32 = 0.0005; // ~3 units
// Used when a device does not report its axis ranges at all.
const SPAN_FALLBACK: f32 = 6750.0;
// Two-finger tap: how long the fingers may stay down for a contact to count as a
// tap (how far they may travel is MOVE_START_FRAC), and how long after one tap a
// second one still makes a double tap. Timed off the device's own event clock, so
// frame rate does not enter into it.
const TAP_MAX_SECS: f64 = 0.25;
const DOUBLE_TAP_SECS: f64 = 0.4;
// Far enough in the past that no tap chains off it.
const TAP_NEVER: f64 = -1000.0;
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
    // Where this finger landed, for measuring how far a tap drifted. Set on the
    // first full position it reports, not at activation, since the slot still
    // holds the previous finger's coordinates then.
    start_x: i32,
    start_y: i32,
    start_set: bool,
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
    // Shorter axis of the device's coordinate span, the reference every distance
    // threshold is a fraction of.
    span: f32,
    slots: [Slot; MAX_SLOTS],
    cur: usize,
    // Baselines for the current two-finger gesture; None when not exactly two
    // fingers are down, so a new gesture never jumps.
    prev_centroid: Option<(f32, f32)>,
    prev_dist: Option<f32>,
    // Which of pan/zoom the gesture is currently applying.
    mode: GestureMode,
    // Centroid and finger distance when the current two-finger gesture began, and
    // whether each has travelled far enough from there to pan or to zoom.
    pan_ref: Option<(f32, f32)>,
    pan_armed: bool,
    zoom_ref: Option<f32>,
    zoom_armed: bool,
    // Contact tracking, driven by the event stream rather than by frames: a
    // contact runs from the first finger down to the last one up. Fingers
    // currently down, when the contact began (device clock), the most fingers it
    // ever had, and the furthest any of them travelled.
    down: usize,
    contact_start: f64,
    contact_max: usize,
    contact_moved: f32,
    // When the last completed tap happened, and whether a double tap is waiting
    // to be consumed by update.
    last_tap: f64,
    tap_reset: bool,
    // OM_WM_DEBUG_TAP=1: report every contact and why it did or did not count as
    // a tap. Trackpad thresholds are hard to guess at, and this turns tuning them
    // into reading numbers.
    debug_taps: bool,
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
    // Not grabbed: in Custom mode libinput already has this device muted, and a
    // grab here would only starve libinput's own handle on it.
    let span = match (abs_span(fd, ABS_MT_POSITION_X), abs_span(fd, ABS_MT_POSITION_Y)) {
        (Some(x), Some(y)) => x.min(y),
        (Some(x), None) => x,
        (None, Some(y)) => y,
        (None, None) => SPAN_FALLBACK,
    };
    println!(
        "om_wm: touchpad {path} (span {span:.0}: pan/tap {:.0}, pinch {:.0} units)",
        span * MOVE_START_FRAC,
        span * PINCH_START_FRAC
    );
    Some(Touchpad {
        fd,
        span,
        slots: [Slot { active: false, x: 0, y: 0, start_x: 0, start_y: 0, start_set: false }; MAX_SLOTS],
        cur: 0,
        prev_centroid: None,
        prev_dist: None,
        mode: GestureMode::None,
        pan_ref: None,
        pan_armed: false,
        zoom_ref: None,
        zoom_armed: false,
        down: 0,
        contact_start: 0.0,
        contact_max: 0,
        contact_moved: 0.0,
        last_tap: TAP_NEVER,
        tap_reset: false,
        debug_taps: std::env::var("OM_WM_DEBUG_TAP").is_ok(),
        primary_slot: None,
        prev_single: None,
        ptr_accum_x: 0.0,
        ptr_accum_y: 0.0,
        btn_left: false,
        click_pressed: false,
        click_released: false,
    })
}

// The travel of one axis in device units, from the driver.
// EVIOCGABS(axis) = _IOR('E', 0x40 + axis, struct input_absinfo).
fn abs_span(fd: i32, axis: u16) -> Option<f32> {
    #[repr(C)]
    #[derive(Default)]
    struct AbsInfo {
        value: i32,
        minimum: i32,
        maximum: i32,
        fuzz: i32,
        flat: i32,
        resolution: i32,
    }
    let size = std::mem::size_of::<AbsInfo>() as libc::c_ulong;
    let req: libc::c_ulong =
        (2 << 30) | (size << 16) | (0x45 << 8) | (0x40 + axis as libc::c_ulong);
    let mut info = AbsInfo::default();
    if unsafe { libc::ioctl(fd, req, &mut info as *mut AbsInfo) } < 0 {
        return None;
    }
    let span = (info.maximum - info.minimum) as f32;
    if span > 0.0 {
        Some(span)
    } else {
        None
    }
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
// Reset
//

// Drain queued events and forget all finger/gesture state, so returning from
// another VT does not replay a stale gesture into the camera.
pub fn reset(tp: &mut Touchpad) {
    read_events(tp);
    tp.slots = [Slot { active: false, x: 0, y: 0, start_x: 0, start_y: 0, start_set: false }; MAX_SLOTS];
    tp.cur = 0;
    tp.prev_centroid = None;
    tp.prev_dist = None;
    tp.mode = GestureMode::None;
    tp.pan_ref = None;
    tp.pan_armed = false;
    tp.zoom_ref = None;
    tp.zoom_armed = false;
    tp.down = 0;
    tp.contact_max = 0;
    tp.contact_moved = 0.0;
    tp.last_tap = TAP_NEVER;
    tp.tap_reset = false;
    tp.primary_slot = None;
    tp.prev_single = None;
    tp.ptr_accum_x = 0.0;
    tp.ptr_accum_y = 0.0;
    tp.btn_left = false;
    tp.click_pressed = false;
    tp.click_released = false;
}

//
// Taps
//

// A contact just ended (the last finger came up). A quick, still, two-finger
// contact is a tap, and two taps in quick succession ask for a zoom reset.
//
// This lives on the event stream instead of the per-frame slot snapshot because
// read_events drains the whole queue at once: a tap that starts and finishes
// between two frames leaves no frame where two fingers look down, so a snapshot
// never sees it at all.
fn end_contact(tp: &mut Touchpad, time: f64) {
    let held = time - tp.contact_start;
    let quick = held <= TAP_MAX_SECS;
    let still = tp.contact_moved <= tp.span * MOVE_START_FRAC;
    let tap = tp.contact_max == 2 && quick && still;
    if tp.debug_taps {
        eprintln!(
            "om_wm: contact fingers={} held={:.0}ms moved={:.0} (limits {:.0}ms, {:.0}) tap={tap} gap={:.0}ms",
            tp.contact_max,
            held * 1000.0,
            tp.contact_moved,
            TAP_MAX_SECS * 1000.0,
            tp.span * MOVE_START_FRAC,
            (time - tp.last_tap) * 1000.0
        );
    }
    if !tap {
        return;
    }
    if time - tp.last_tap <= DOUBLE_TAP_SECS {
        tp.tap_reset = true;
        // Consumed, so a third tap does not reset again.
        tp.last_tap = TAP_NEVER;
    } else {
        tp.last_tap = time;
    }
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

        // The event's own timeval, which is what tap timing runs on.
        let mut secs = [0u8; 8];
        let mut usecs = [0u8; 8];
        secs.copy_from_slice(&buf[0..8]);
        usecs.copy_from_slice(&buf[8..16]);
        let time =
            i64::from_ne_bytes(secs) as f64 + i64::from_ne_bytes(usecs) as f64 * 1e-6;

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
                if tp.cur >= MAX_SLOTS {
                    continue;
                }
                let was = tp.slots[tp.cur].active;
                let now = value != -1;
                tp.slots[tp.cur].active = now;
                if now && !was {
                    tp.slots[tp.cur].start_set = false;
                    if tp.down == 0 {
                        tp.contact_start = time;
                        tp.contact_max = 0;
                        tp.contact_moved = 0.0;
                    }
                    tp.down += 1;
                    tp.contact_max = tp.contact_max.max(tp.down);
                }
                if was && !now {
                    tp.down = tp.down.saturating_sub(1);
                    if tp.down == 0 {
                        end_contact(tp, time);
                    }
                }
            }
            (EV_ABS, ABS_MT_POSITION_X) => {
                if tp.cur < MAX_SLOTS {
                    tp.slots[tp.cur].x = value;
                }
            }
            // Y closes a slot's position packet (X comes first in protocol B), so
            // this is where a landing position is recorded and drift measured.
            (EV_ABS, ABS_MT_POSITION_Y) => {
                if tp.cur >= MAX_SLOTS {
                    continue;
                }
                tp.slots[tp.cur].y = value;
                let slot = tp.slots[tp.cur];
                if !slot.start_set {
                    tp.slots[tp.cur].start_x = slot.x;
                    tp.slots[tp.cur].start_y = slot.y;
                    tp.slots[tp.cur].start_set = true;
                } else {
                    let dx = (slot.x - slot.start_x) as f32;
                    let dy = (slot.y - slot.start_y) as f32;
                    tp.contact_moved = tp.contact_moved.max((dx * dx + dy * dy).sqrt());
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

    // Two-finger double tap resets the zoom. read_events decided that off the
    // event stream; all that is left is to honour it.
    if tp.tap_reset {
        tp.tap_reset = false;
        if gestures_enabled {
            camera::reset_zoom(cam);
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

    // Not a two-finger gesture: nothing to pan or zoom. Panning stops dead when
    // the fingers lift, no glide.
    if count != 2 {
        tp.prev_centroid = None;
        tp.prev_dist = None;
        tp.mode = GestureMode::None;
        tp.pan_ref = None;
        tp.pan_armed = false;
        tp.zoom_ref = None;
        tp.zoom_armed = false;
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

    // Pan and zoom each have to travel a minimum distance from where the gesture
    // began before they do anything: a two-finger tap, a resting hand or a hand
    // settling on the pad should move neither. Arming re-baselines, so the motion
    // starts from there rather than jumping by the whole threshold. The pan
    // threshold is also the drift a tap is allowed, so a contact either pans or
    // stays eligible as a tap.
    let move_start = tp.span * MOVE_START_FRAC;
    let pinch_start = tp.span * PINCH_START_FRAC;
    match tp.pan_ref {
        None => {
            tp.pan_ref = Some(centroid);
            tp.pan_armed = false;
        }
        Some(r) if !tp.pan_armed => {
            let moved = ((centroid.0 - r.0).powi(2) + (centroid.1 - r.1).powi(2)).sqrt();
            if moved >= move_start {
                tp.pan_armed = true;
                tp.prev_centroid = Some(centroid);
            }
        }
        _ => {}
    }
    match tp.zoom_ref {
        None => {
            tp.zoom_ref = Some(dist);
            tp.zoom_armed = false;
        }
        Some(r) if !tp.zoom_armed && (dist - r).abs() >= pinch_start => {
            tp.zoom_armed = true;
            tp.prev_dist = Some(dist);
        }
        _ => {}
    }

    match (tp.prev_centroid, tp.prev_dist) {
        (Some(pc), Some(pd)) => {
            let pan_delta =
                ((centroid.0 - pc.0).powi(2) + (centroid.1 - pc.1).powi(2)).sqrt();
            let zoom_delta = (dist - pd).abs();

            // Pick the dominant intent, with hysteresis: only switch out of the
            // current mode when the other motion clearly leads.
            let mode_eps = tp.span * MODE_EPS_FRAC;
            let want_zoom = tp.zoom_armed
                && zoom_delta > mode_eps
                && zoom_delta > pan_delta * MODE_BIAS;
            let want_pan = tp.pan_armed
                && pan_delta > mode_eps
                && pan_delta > zoom_delta * MODE_BIAS;
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
                }
                GestureMode::Zoom => {
                    let deadzone = tp.span * PINCH_DEADZONE_FRAC;
                    let ddist = dist - pd;
                    if ddist.abs() > deadzone {
                        let beyond = ddist - ddist.signum() * deadzone;
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
            // Gesture start: no baselines yet, so nothing to apply this frame.
            tp.mode = GestureMode::None;
        }
    }

    tp.prev_centroid = Some(centroid);
    tp.prev_dist = Some(dist);
    clicks
}
