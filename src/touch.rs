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

use crate::cursor::{self, Cursor};
use crate::settings::Settings;

//
// Constants (evdev)
//

const EV_ABS: u16 = 0x03;
const EV_KEY: u16 = 0x01;
const ABS_MT_SLOT: u16 = 0x2F;
const ABS_MT_POSITION_X: u16 = 0x35;
const ABS_MT_POSITION_Y: u16 = 0x36;
const ABS_MT_TRACKING_ID: u16 = 0x39;
// Contact size per finger: the ellipse the finger presses into the surface. This pad
// reports no per-finger pressure (no ABS_MT_PRESSURE), and size is the usual stand-in,
// since a finger flattens as it presses.
const ABS_MT_TOUCH_MAJOR: u16 = 0x30;
const ABS_MT_TOUCH_MINOR: u16 = 0x31;
// Total load on the pad, one value for the whole device rather than per finger.
const ABS_PRESSURE: u16 = 0x18;
const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
// INPUT_PROP_BUTTONPAD is property bit 2: the whole surface is one physical button.
const INPUT_PROP_BUTTONPAD: usize = 2;

// struct input_event is 24 bytes on 64-bit Linux: 16-byte timeval, u16 type,
// u16 code, i32 value.
const EVENT_SIZE: usize = 24;

const MAX_SLOTS: usize = 16;

// Feel (these are the natural home for future config settings):
// canvas pixels panned per trackpad device unit at zoom 1.0. Divided by zoom so
// panning feels the same at any scale.
// A two-finger gesture is pan OR zoom at any instant, never both. The active
// mode is sticky and switches only when the other motion dominates by this
// factor (hysteresis), so pan and zoom stay clean but interchange fluidly
// without lifting.

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
// How much the finger distance must change, in total from where the gesture
// began, before a pinch zooms at all. Below this the gesture is a tap, a resting
// hand or a pan, none of which should nudge the zoom.
// Minimum per-frame motion before pan-versus-zoom dominance is judged.
// Per-frame pinch jitter ignored once armed.
// Used when a device does not report its axis ranges at all.
const SPAN_FALLBACK: f32 = 6750.0;
// Two-finger tap: how long the fingers may stay down for a contact to count as a
// tap (how far they may travel is move_start_frac), and how long after one tap a
// second one still makes a double tap. Timed off the device's own event clock, so
// frame rate does not enter into it.
// Far enough in the past that no tap chains off it.
const TAP_NEVER: f64 = -1000.0;
// One-finger pointer motion: screen pixels moved per trackpad device unit.

// How far a finger has to travel before it moves the cursor at all, as a fraction of
// the pad. A finger landing rolls and spreads, and pressing the pad down slides it, so
// without this the cursor drifts off target in the moment before a click lands.
// And how long after a press the cursor stays parked regardless, for the roll as the
// finger takes the pad's travel. Longer than this and a click-drag would feel stuck.

// Software button strip on a clickpad: the bottom third of the surface, split in half.
// A click with a finger in the right half of that strip is a right click, everything
// else is a left click. Only used on hardware that has no buttons of its own.

//
// Types
//

#[derive(Clone, Copy)]
struct Slot {
    active: bool,
    // The kernel's id for the finger in this slot, or -1 for empty.
    //
    // Kept because active is not enough to tell one finger from another. Multitouch protocol B
    // may hand a slot a new tracking id without passing through -1, which is one finger leaving
    // and another arriving in the same breath, and everything measured from where "this slot"
    // was a frame ago is measured from the wrong finger.
    id: i32,
    x: i32,
    y: i32,
    // When this finger landed, counted rather than timed: the pointer picks the
    // earliest non-resting finger, and slots are reused, so the slot index says
    // nothing about order of appearance.
    seq: u32,
    // Where the finger was when it last moved meaningfully, and when that was. A
    // finger that has not moved from here for rest_secs is parked, not pointing.
    still_x: i32,
    still_y: i32,
    still_since: f64,
    // Contact ellipse, in the same device units as the positions: how much of the finger
    // is touching. Zero on hardware that does not report it.
    major: i32,
    minor: i32,
    // Too little contact to count as a touch at all: a finger grazing the surface, or
    // the edge of one that is mostly off it. Sticky in both directions, see
    // classify_fingers.
    faint: bool,
    // Parked: in the resting zone and idle. Held across frames because it must not
    // flicker, and frozen while a gesture is running (see classify_resting).
    resting: bool,
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
    // Next appearance number to hand a finger that lands.
    seq_next: u32,
    // Shorter axis of the device's coordinate span, the reference every distance
    // threshold is a fraction of.
    span: f32,
    // Full extent of each axis, for locating a finger on the surface as a fraction
    // rather than in device units, which differ per model.
    x_min: f32,
    x_span: f32,
    y_min: f32,
    y_span: f32,
    // Full extent of the contact size axis, and of the whole-pad load axis. Zero when
    // the device does not report them, which the overlay has to respect rather than
    // draw a footprint it does not know.
    size_max: f32,
    load_max: f32,
    // Latest whole-pad load reading.
    load: i32,
    // True when this device has no buttons of its own and we have to make them out of
    // regions (see button_strip). Read from the kernel at open, not guessed.
    software_buttons: bool,
    slots: [Slot; MAX_SLOTS],
    cur: usize,
    // Baselines for the current two-finger gesture; None when not exactly two
    // fingers are down, so a new gesture never jumps.
    prev_centroid: Option<(f32, f32)>,
    prev_dist: Option<f32>,
    // Which of pan/zoom the gesture is currently applying.
    mode: GestureMode,
    // Where three fingers were when they landed, and whether they have already been counted
    // as a swipe. Latched so a hand that keeps travelling only fires once.
    swipe_ref: Option<(f32, f32)>,
    swiped: bool,
    // Whether the two-finger gesture now in progress has zoomed, or panned, at any point, so
    // that the end of the gesture can be reported as the end of each. Not simply the previous
    // frame's mode: a gesture is allowed to move between panning and zooming while the
    // fingers stay down, and neither is over until they lift.
    zoomed_gesture: bool,
    panned_gesture: bool,
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
    contact_max: usize,
    // When the last completed tap happened, and whether a double tap is waiting
    // to be consumed by update.
    // OM_WM_DEBUG_TAP=1: report every contact and why it did or did not count as
    // a tap. Trackpad thresholds are hard to guess at, and this turns tuning them
    // into reading numbers.
    debug_taps: bool,
    // Report any single frame that moves the cursor further than a hand could have, with the
    // state that produced it. For the teleport that only happens sometimes: it has to be caught
    // in the act, because everything about it looks ordinary a frame later.
    debug_jumps: bool,
    // Pointer tracking: which slot drives the cursor, its previous pos, and the
    // fractional remainder for slow motion.
    primary_slot: Option<usize>,
    prev_single: Option<(f32, f32)>,
    ptr_accum_x: f32,
    ptr_accum_y: f32,
    // Where the cursor was parked and whether the finger has since travelled far
    // enough to drive it. Re-parked on landing and on every press and release.
    ptr_ref: Option<(f32, f32)>,
    ptr_armed: bool,
    // Latest event time, and when the button last went down, for the press freeze.
    now: f64,
    press_time: f64,
    // The trackpad's one physical button: current level, and which button it counts as
    // this time round. The choice is made on the press and held until the release,
    // because a finger can slide out of the strip while still holding the pad down, and
    // a press and its release have to be the same button.
    btn_down: bool,
    right_click: bool,
    clicks: Clicks,
    // What the last gesture frame applied, kept only so the debug overlay can report
    // what is being detected rather than what we hope is being detected.
    last_pan: (f32, f32),
    last_zoom: f32,
}

// A read-only snapshot of the pad for the debug overlay: fingers as fractions of the
// surface, the button regions, and what the gesture code currently believes.
pub struct PadView {
    pub fingers: [(f32, f32); MAX_SLOTS],
    // Contact ellipse per finger, as a fraction of the pad's height so it scales with
    // the drawing, plus the raw major value for reading off numbers.
    pub size: [(f32, f32); MAX_SLOTS],
    pub major: [i32; MAX_SLOTS],
    // Which of those fingers are parked rather than pointing, in the same order.
    pub resting: [bool; MAX_SLOTS],
    // Contacts too small to count at all, in the same order.
    pub faint: [bool; MAX_SLOTS],
    pub count: usize,
    // How many of them are not parked: what the gesture code is actually working with.
    pub active: usize,
    pub rest_zone: f32,
    pub contact_max: usize,
    pub left: bool,
    pub right: bool,
    pub mode: &'static str,
    pub pan_armed: bool,
    pub zoom_armed: bool,
    pub ptr_armed: bool,
    pub software_buttons: bool,
    pub strip: f32,
    pub split: f32,
    pub aspect: f32,
    pub pan: (f32, f32),
    pub zoom: f32,
    // Whether this device reports contact size at all, and the whole-pad load.
    pub has_size: bool,
    pub load: i32,
    pub load_max: i32,
}

// Everything the pad did this frame, in the same shape libinput reports for a trackpad
// it drives itself: scroll in pixels, pinch as a factor, buttons as edges and levels.
//
// Reported rather than applied. This module used to pan and zoom the camera directly,
// which meant a two-finger scroll existed only as a camera mutation that had already
// happened, and there was no value left for the caller to send to a client instead. The
// caller now decides where it goes, exactly as it already does for libinput's version, so
// the same scroll can reach the canvas or the window under the pointer.
#[derive(Clone, Copy)]
pub struct Gesture {
    pub clicks: Clicks,
    // Two-finger scroll, in canvas pixels. Positive is up and right, matching
    // input::Pointer, so the caller can add the two together without thinking.
    pub scroll_x: f32,
    pub scroll_y: f32,
    // Pinch factor for this frame, 1.0 for none.
    pub pinch: f32,
    // True on the one frame a pinch stops, however it stopped: fingers lifted, a hand came
    // to rest, or the gesture turned into a pan. A paused pinch reports a factor of 1.0
    // exactly like a finished one, so the caller cannot tell them apart and needs telling.
    pub zoom_ended: bool,
    // A pinch has begun. Wayland's gesture protocol is a sequence with a beginning, a
    // middle and an end, so the beginning has to be an event of its own.
    pub zoom_started: bool,
    // The two-finger scroll that was running has stopped, because the fingers lifted or the
    // gesture turned into something else. Wayland requires saying so for a finger source: the
    // client cannot see the pad, so an axis sequence that is never ended is one it goes on
    // believing is in progress.
    pub scroll_ended: bool,
    // How many contacts are on the pad at all, faint and parked included. Not what a gesture
    // is made of: what it means is "is the hand still there". A scroll's destination is decided
    // once and has to hold for the whole gesture, and scroll_ended above cannot answer that
    // question, since it fires the moment the pan stops leading, which is what a swipe does as
    // it slows down before lifting.
    pub fingers: usize,
    // Three fingers went up, or down. Fires once, on the frame the travel passes the
    // threshold, and says nothing else: an instruction rather than a motion to follow.
    pub swipe_up: bool,
    pub swipe_down: bool,
}

impl Default for Gesture {
    fn default() -> Self {
        Gesture {
            clicks: Clicks::default(),
            scroll_x: 0.0,
            scroll_y: 0.0,
            pinch: 1.0,
            zoom_started: false,
            zoom_ended: false,
            scroll_ended: false,
            fingers: 0,
            swipe_up: false,
            swipe_down: false,
        }
    }
}

// Button edges and levels from the pad this frame, shaped like the mouse's so that
// everything downstream can treat a clickpad and a two button mouse identically.
#[derive(Clone, Copy, Default)]
pub struct Clicks {
    pub left_pressed: bool,
    pub left_released: bool,
    pub left: bool,
    pub right_pressed: bool,
    pub right_released: bool,
    pub right: bool,
}

//
// Open
//

// The node comes from libinput, which already knows which device is a touchpad,
// so there is no discovery here to get wrong.
pub fn open(path: &str, set: &Settings) -> Option<Touchpad> {
    let c = CString::new(path).ok()?;
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_NONBLOCK) };
    if fd < 0 {
        eprintln!("om_wm: touchpad open failed: {path}");
        return None;
    }
    // Not grabbed: in Custom mode libinput already has this device muted, and a
    // grab here would only starve libinput's own handle on it.
    let x_axis = abs_range(fd, ABS_MT_POSITION_X);
    let y_axis = abs_range(fd, ABS_MT_POSITION_Y);
    let span = match (x_axis, y_axis) {
        (Some((_, x)), Some((_, y))) => x.min(y),
        (Some((_, x)), None) => x,
        (None, Some((_, y))) => y,
        (None, None) => SPAN_FALLBACK,
    };
    let (x_min, x_span) = x_axis.unwrap_or((0.0, SPAN_FALLBACK));
    let (y_min, y_span) = y_axis.unwrap_or((0.0, SPAN_FALLBACK));
    // Two questions, both answered by the kernel: does the surface sit under a single
    // button, and is there a right button anywhere on the device. A real right button
    // settles it on its own, whatever the property says.
    let size_max = abs_range(fd, ABS_MT_TOUCH_MAJOR).map(|(_, span)| span).unwrap_or(0.0);
    let load_max = abs_range(fd, ABS_PRESSURE).map(|(_, span)| span).unwrap_or(0.0);
    let buttonpad = has_property(fd, INPUT_PROP_BUTTONPAD);
    let has_right = has_key(fd, BTN_RIGHT);
    let software_buttons = buttonpad && !has_right;
    println!(
        "om_wm: touchpad {path} (span {span:.0}: pan/tap {:.0}, pinch {:.0} units)",
        span * set.move_start_frac,
        span * set.pinch_start_frac
    );
    println!(
        "om_wm: touchpad contact size {}, whole-pad load {}",
        if size_max > 0.0 { "reported" } else { "not reported" },
        if load_max > 0.0 { "reported" } else { "not reported" }
    );
    println!(
        "om_wm: touchpad buttons: {}",
        if software_buttons {
            "clickpad, bottom third of the surface split in half (left | right)"
        } else if has_right {
            "physical left and right"
        } else {
            "one button, and the kernel does not call it a clickpad: left only"
        }
    );
    Some(Touchpad {
        fd,
        span,
        x_min,
        x_span,
        y_min,
        y_span,
        software_buttons,
        size_max,
        load_max,
        load: 0,
        slots: [Slot {
            active: false,
            id: -1,
            x: 0,
            y: 0,
            seq: 0,
            still_x: 0,
            still_y: 0,
            still_since: 0.0,
            major: 0,
            minor: 0,
            faint: false,
            resting: false,
            start_x: 0,
            start_y: 0,
            start_set: false,
        }; MAX_SLOTS],
        seq_next: 0,
        cur: 0,
        prev_centroid: None,
        prev_dist: None,
        mode: GestureMode::None,
        swipe_ref: None,
        swiped: false,
        zoomed_gesture: false,
        panned_gesture: false,
        pan_ref: None,
        pan_armed: false,
        zoom_ref: None,
        zoom_armed: false,
        down: 0,
        contact_max: 0,
        debug_taps: std::env::var("OM_WM_DEBUG_TAP").is_ok(),
        debug_jumps: std::env::var("OM_WM_DEBUG_JUMP").is_ok(),
        primary_slot: None,
        prev_single: None,
        ptr_accum_x: 0.0,
        ptr_accum_y: 0.0,
        ptr_ref: None,
        ptr_armed: false,
        now: 0.0,
        press_time: TAP_NEVER,
        btn_down: false,
        right_click: false,
        clicks: Clicks::default(),
        last_pan: (0.0, 0.0),
        last_zoom: 1.0,
    })
}

// Where one axis starts and how far it travels, in device units, from the driver.
// EVIOCGABS(axis) = _IOR('E', 0x40 + axis, struct input_absinfo).
fn abs_range(fd: i32, axis: u16) -> Option<(f32, f32)> {
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
        Some((info.minimum as f32, span))
    } else {
        None
    }
}

// Whether the device reports a property bit. EVIOCGPROP(len) = _IOR('E', 0x09, len).
fn has_property(fd: i32, prop: usize) -> bool {
    let mut bits = [0u8; 4];
    let req: libc::c_ulong =
        (2 << 30) | ((bits.len() as libc::c_ulong) << 16) | (0x45 << 8) | 0x09;
    if unsafe { libc::ioctl(fd, req, bits.as_mut_ptr()) } < 0 {
        return false;
    }
    bit_set(&bits, prop)
}

// Whether the device can report a key or button code. EVIOCGBIT(EV_KEY, len) =
// _IOR('E', 0x20 + EV_KEY, len).
fn has_key(fd: i32, code: u16) -> bool {
    // KEY_MAX is 0x2ff, so this covers every code the kernel can report.
    let mut bits = [0u8; 96];
    let req: libc::c_ulong = (2 << 30)
        | ((bits.len() as libc::c_ulong) << 16)
        | (0x45 << 8)
        | (0x20 + EV_KEY as libc::c_ulong);
    if unsafe { libc::ioctl(fd, req, bits.as_mut_ptr()) } < 0 {
        return false;
    }
    bit_set(&bits, code as usize)
}

fn bit_set(bits: &[u8], n: usize) -> bool {
    let byte = n / 8;
    byte < bits.len() && bits[byte] & (1 << (n % 8)) != 0
}

// Close the device, for when it is unplugged or libinput hands us a different
// one.
pub fn close(tp: &mut Touchpad) {
    if tp.fd >= 0 {
        unsafe { libc::close(tp.fd) };
        tp.fd = -1;
    }
}

//
// Reset
//

// Drain queued events and forget all finger/gesture state, so returning from
// another VT does not replay a stale gesture into the camera.
pub fn reset(tp: &mut Touchpad, set: &Settings) {
    read_events(tp, set);
    tp.slots = [Slot {
        active: false,
        id: -1,
        x: 0,
        y: 0,
        seq: 0,
        still_x: 0,
        still_y: 0,
        still_since: 0.0,
        major: 0,
        minor: 0,
        faint: false,
        resting: false,
        start_x: 0,
        start_y: 0,
        start_set: false,
    }; MAX_SLOTS];
    tp.cur = 0;
    tp.prev_centroid = None;
    tp.prev_dist = None;
    tp.mode = GestureMode::None;
    tp.swipe_ref = None;
    tp.swiped = false;
    tp.pan_ref = None;
    tp.pan_armed = false;
    tp.zoom_ref = None;
    tp.zoom_armed = false;
    tp.down = 0;
    tp.contact_max = 0;
    tp.primary_slot = None;
    tp.prev_single = None;
    tp.ptr_accum_x = 0.0;
    tp.ptr_accum_y = 0.0;
    tp.ptr_ref = None;
    tp.ptr_armed = false;
    tp.press_time = TAP_NEVER;
    tp.btn_down = false;
    tp.right_click = false;
    tp.clicks = Clicks::default();
}

// A snapshot for the debug overlay. Fractions of the surface rather than device units,
// so the drawing code needs to know nothing about this model of trackpad.
pub fn view(tp: &Touchpad, set: &Settings) -> PadView {
    let mut fingers = [(0.0f32, 0.0f32); MAX_SLOTS];
    let mut size = [(0.0f32, 0.0f32); MAX_SLOTS];
    let mut major = [0i32; MAX_SLOTS];
    let mut resting = [false; MAX_SLOTS];
    let mut faint = [false; MAX_SLOTS];
    let mut count = 0;
    let mut active = 0;
    for slot in &tp.slots {
        if slot.active {
            fingers[count] = (
                (slot.x as f32 - tp.x_min) / tp.x_span,
                (slot.y as f32 - tp.y_min) / tp.y_span,
            );
            // Same units as the positions, so dividing by the same span puts the
            // footprint at the same scale as the dot.
            size[count] = (slot.major as f32 / tp.y_span, slot.minor as f32 / tp.y_span);
            major[count] = slot.major;
            resting[count] = slot.resting;
            faint[count] = slot.faint;
            if !slot.resting && !slot.faint {
                active += 1;
            }
            count += 1;
        }
    }
    PadView {
        fingers,
        size,
        major,
        resting,
        faint,
        count,
        active,
        rest_zone: set.rest_zone_frac,
        contact_max: tp.contact_max,
        left: tp.btn_down && !tp.right_click,
        right: tp.btn_down && tp.right_click,
        mode: match tp.mode {
            GestureMode::None => "none",
            GestureMode::Pan => "pan",
            GestureMode::Zoom => "zoom",
        },
        pan_armed: tp.pan_armed,
        zoom_armed: tp.zoom_armed,
        ptr_armed: tp.ptr_armed,
        software_buttons: tp.software_buttons,
        strip: set.button_strip,
        split: set.button_split,
        aspect: tp.x_span / tp.y_span,
        pan: tp.last_pan,
        zoom: tp.last_zoom,
        has_size: tp.size_max > 0.0,
        load: tp.load,
        load_max: tp.load_max as i32,
    }
}

//
// Taps
//


//
// Update
//

fn read_events(tp: &mut Touchpad, set: &Settings) {
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

        tp.now = time;
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
                // One finger swapped for another in the same slot, with no -1 in between. The
                // slot is as new as one that just landed, even though it never went empty.
                let replaced = was && now && tp.slots[tp.cur].id != value;
                tp.slots[tp.cur].active = now;
                tp.slots[tp.cur].id = value;
                if now && (!was || replaced) {
                    tp.slots[tp.cur].start_set = false;
                    tp.slots[tp.cur].seq = tp.seq_next;
                    tp.seq_next = tp.seq_next.wrapping_add(1);
                    tp.slots[tp.cur].still_since = time;
                    tp.slots[tp.cur].still_x = tp.slots[tp.cur].x;
                    tp.slots[tp.cur].still_y = tp.slots[tp.cur].y;
                    tp.slots[tp.cur].resting = false;
                    // A replacement is a landing and a lift at once, so the count is unchanged.
                    if !was {
                        if tp.down == 0 {
                            tp.contact_max = 0;
                        }
                        tp.down += 1;
                        tp.contact_max = tp.contact_max.max(tp.down);
                    }
                }
                if replaced {
                    // Everything measured from where a finger was is now measured from a
                    // different finger. The pointer would step by the gap between the two, which
                    // is the cursor teleporting across the screen, and the centroid would take
                    // the canvas with it. Drop the baselines and let them be retaken: the
                    // pointer re-arms after its usual travel, so nothing moves until you mean it.
                    if tp.primary_slot == Some(tp.cur) {
                        tp.prev_single = None;
                        tp.ptr_ref = None;
                        tp.ptr_armed = false;
                    }
                    tp.prev_centroid = None;
                    tp.prev_dist = None;
                    tp.pan_ref = None;
                    tp.pan_armed = false;
                    tp.zoom_ref = None;
                    tp.zoom_armed = false;
                    tp.swipe_ref = None;
                }
                if was && !now {
                    tp.down = tp.down.saturating_sub(1);
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
                }
                // Idle tracking: the clock only restarts when the finger leaves a small
                // circle, so the noise a still finger reports does not keep it awake.
                let sx = (slot.x - slot.still_x) as f32;
                let sy = (slot.y - slot.still_y) as f32;
                if (sx * sx + sy * sy).sqrt() > tp.span * set.rest_move_frac {
                    tp.slots[tp.cur].still_x = slot.x;
                    tp.slots[tp.cur].still_y = slot.y;
                    tp.slots[tp.cur].still_since = time;
                    tp.slots[tp.cur].resting = false;
                }
            }
            (EV_ABS, ABS_MT_TOUCH_MAJOR) => {
                if tp.cur < MAX_SLOTS {
                    tp.slots[tp.cur].major = value;
                }
            }
            (EV_ABS, ABS_MT_TOUCH_MINOR) => {
                if tp.cur < MAX_SLOTS {
                    tp.slots[tp.cur].minor = value;
                }
            }
            (EV_ABS, ABS_PRESSURE) => {
                tp.load = value;
            }
            (EV_KEY, BTN_LEFT) => {
                // Park the cursor around the click, at both edges: the finger rolls as
                // it presses and again as it lifts, and neither should move the pointer
                // away from what is being clicked.
                tp.press_time = time;
                tp.ptr_ref = None;
                tp.ptr_armed = false;
                if value == 1 {
                    tp.btn_down = true;
                    tp.right_click = press_is_right(tp, set);
                    if tp.right_click {
                        tp.clicks.right_pressed = true;
                    } else {
                        tp.clicks.left_pressed = true;
                    }
                } else if value == 0 {
                    tp.btn_down = false;
                    if tp.right_click {
                        tp.clicks.right_released = true;
                    } else {
                        tp.clicks.left_released = true;
                    }
                    tp.right_click = false;
                }
            }
            // A pad with real buttons reports its own right click, so pass it through.
            (EV_KEY, BTN_RIGHT) => {
                if value == 1 {
                    tp.clicks.right_pressed = true;
                    tp.right_click = true;
                } else if value == 0 {
                    tp.clicks.right_released = true;
                    tp.right_click = false;
                }
            }
            _ => {}
        }
    }
}

// Decide which fingers are parked rather than pointing.
//
// A hand rests on a trackpad: a thumb sits in the button area waiting to click, or a
// finger just stays put. Those fingers should not steer the cursor, should not count
// toward the two-finger gesture test, and should not decide which button a click is.
// Anything else means a resting thumb turns every one-finger move into a pan and every
// right click into a left one.
//
// Parked means both in the resting zone (the bottom of the pad, where a hand naturally
// rests, and where the software buttons are) and idle for rest_secs. Both halves matter:
// the zone alone would park a finger that is pointing slowly down there, and idleness
// alone would park the pivot finger of a pinch.
//
// Frozen while a gesture is running. A pinch can hold one finger still for longer than
// rest_secs, and reclassifying mid-gesture would drop that finger, take the gesture down
// to one finger, and hand the cursor a jump.
fn classify_fingers(tp: &mut Touchpad, set: &Settings) {
    // Faintness first, and every frame: it is about whether a finger is there at all,
    // not what it means, and a finger that is genuinely lifting should end a gesture
    // rather than be held onto. The two thresholds are what keep a contact resting on
    // the boundary from flickering: it has to reach touch_min_size to start counting and
    // fall below touch_drop_size to stop.
    if set.touch_min_size > 0.0 && tp.size_max > 0.0 {
        for i in 0..MAX_SLOTS {
            if !tp.slots[i].active {
                tp.slots[i].faint = false;
                continue;
            }
            let major = tp.slots[i].major as f32;
            tp.slots[i].faint = if tp.slots[i].faint {
                major < set.touch_min_size
            } else {
                major < set.touch_drop_size
            };
        }
    }

    if tp.pan_armed || tp.zoom_armed || tp.mode != GestureMode::None {
        return;
    }
    for i in 0..MAX_SLOTS {
        if !tp.slots[i].active {
            tp.slots[i].resting = false;
            continue;
        }
        let fy = (tp.slots[i].y as f32 - tp.y_min) / tp.y_span;
        let in_zone = fy >= 1.0 - set.rest_zone_frac;
        let idle = tp.now - tp.slots[i].still_since >= set.rest_secs;
        tp.slots[i].resting = in_zone && idle;
    }
}

// The finger that should drive the cursor: the earliest one that is not parked, so a
// thumb already resting when you start pointing is skipped rather than followed. If
// every finger is parked, the earliest one stands, since something has to.
//
// While the pad is being held down, a finger inside the button strip is skipped too. A
// click-drag on a clickpad is two contacts by construction, a thumb pressing at the
// bottom and a finger moving above it, and following the thumb means the cursor sits
// still while you drag. The thumb is often too fresh to have been parked yet, so its
// position is what identifies it.
fn pointer_slot(tp: &Touchpad, set: &Settings) -> Option<usize> {
    let mut best: Option<usize> = None;
    for i in 0..MAX_SLOTS {
        if !tp.slots[i].active || tp.slots[i].resting || tp.slots[i].faint {
            continue;
        }
        if tp.btn_down && in_button_strip(tp, i, set) {
            continue;
        }
        if best.map_or(true, |b| tp.slots[i].seq < tp.slots[b].seq) {
            best = Some(i);
        }
    }
    // Nothing above the strip: the pressing finger is also the pointing one.
    if best.is_none() {
        for i in 0..MAX_SLOTS {
            if !tp.slots[i].active || tp.slots[i].resting || tp.slots[i].faint {
                continue;
            }
            if best.map_or(true, |b| tp.slots[i].seq < tp.slots[b].seq) {
                best = Some(i);
            }
        }
    }
    if best.is_some() {
        return best;
    }
    let mut fallback: Option<usize> = None;
    for i in 0..MAX_SLOTS {
        if tp.slots[i].active
            && fallback.map_or(true, |b| tp.slots[i].seq < tp.slots[b].seq)
        {
            fallback = Some(i);
        }
    }
    fallback
}

// Whether a finger is down in the region a clickpad's software buttons occupy.
fn in_button_strip(tp: &Touchpad, i: usize, set: &Settings) -> bool {
    if !tp.software_buttons {
        return false;
    }
    let fy = (tp.slots[i].y as f32 - tp.y_min) / tp.y_span;
    fy >= 1.0 - set.button_strip
}

// Which button a press on the pad's single button counts as. On hardware with real
// buttons the question does not arise. On a clickpad, the finger doing the pressing
// decides: the lowest one on the surface, since with a second finger resting higher up
// it is the low one that is on the button. In the right half of the bottom strip it is a
// right click, anywhere else a left one.
fn press_is_right(tp: &Touchpad, set: &Settings) -> bool {
    if !tp.software_buttons {
        return false;
    }
    // The lowest finger that is actually doing something. A thumb parked in the strip
    // is not what decides the button, or a right click with the index finger while the
    // thumb rests on the left would come out left.
    let mut lowest: Option<(i32, i32)> = None;
    for slot in &tp.slots {
        if slot.active && !slot.resting && !slot.faint && lowest.map_or(true, |(y, _)| slot.y > y) {
            lowest = Some((slot.y, slot.x));
        }
    }
    if lowest.is_none() {
        for slot in &tp.slots {
            if slot.active && lowest.map_or(true, |(y, _)| slot.y > y) {
                lowest = Some((slot.y, slot.x));
            }
        }
    }
    // No finger reported: nothing can be pressing the pad, so take the safe answer.
    let Some((y, x)) = lowest else {
        return false;
    };
    let fy = (y as f32 - tp.y_min) / tp.y_span;
    let fx = (x as f32 - tp.x_min) / tp.x_span;
    let right = fy >= 1.0 - set.button_strip && fx >= set.button_split;
    if tp.debug_taps {
        eprintln!(
            "om_wm: touchpad click at {fx:.2},{fy:.2} of the surface -> {}",
            if right { "right" } else { "left" }
        );
    }
    right
}

// Drain the trackpad and report what the fingers did: one finger moves the cursor, two
// fingers scroll or pinch. The cursor is still moved here, because it is ours to move and
// nothing else wants a say; the scroll and the pinch are handed back for the caller to
// route.
// A finger is a pointing finger rather than part of a gesture whenever the pad is held down,
// which is what stops a click-drag turning into a scroll. It used to also be true while Super
// was held, back when Super meant the canvas was not listening. Super now means the opposite,
// so the pad reports what the fingers did and the caller decides where it goes.
//
// The old text, kept because the caller still has to make that decision: the caller
// sets it while Super is held, which is window manipulation and never a scroll, and it is
// also true whenever the pad's own button is down, since dragging is not scrolling.
// The gesture this frame, plus the edge for a pinch that has just finished. The edge is
// computed out here rather than inside, because the body leaves by half a dozen different
// paths (fingers lifted, a hand rested, the pad held for a drag) and every one of them ends
// a gesture. Reading the mode once, after whichever path was taken, cannot miss one.
pub fn update(tp: &mut Touchpad, cursor: Option<&mut Cursor>, set: &Settings) -> Gesture {
    let mut out = update_gesture(tp, cursor, set);
    let was_zooming = tp.zoomed_gesture;
    if tp.mode == GestureMode::Zoom {
        tp.zoomed_gesture = true;
    }
    out.zoom_started = tp.zoomed_gesture && !was_zooming;
    if tp.mode == GestureMode::Pan {
        tp.panned_gesture = true;
    }
    // Fires once the gesture has stopped altogether, not merely stopped zooming. Turning a
    // pinch into a pan without lifting is one gesture, and springing the zoom back while
    // two fingers are still moving would pull the canvas out from under them.
    out.zoom_ended = tp.zoomed_gesture && tp.mode == GestureMode::None;
    if out.zoom_ended {
        tp.zoomed_gesture = false;
    }
    out.scroll_ended = tp.panned_gesture && tp.mode == GestureMode::None;
    if out.scroll_ended {
        tp.panned_gesture = false;
    }
    out.fingers = tp.down;
    out
}

fn update_gesture(tp: &mut Touchpad, cursor: Option<&mut Cursor>, set: &Settings) -> Gesture {
    tp.clicks = Clicks::default();
    read_events(tp, set);
    // Levels, after the edges: held down and it is still whichever button it became.
    tp.clicks.left = tp.btn_down && !tp.right_click;
    tp.clicks.right = tp.btn_down && tp.right_click;
    let mut out = Gesture { clicks: tp.clicks, ..Gesture::default() };

    // Fingers that are actually doing something. A hand resting on the pad must not
    // make a one-finger move look like a two-finger gesture, so parked fingers are not
    // counted and not used as gesture points.
    classify_fingers(tp, set);
    let mut pts: [(f32, f32); 3] = [(0.0, 0.0); 3];
    let mut count = 0usize;
    for s in &tp.slots {
        if s.active && !s.resting && !s.faint {
            if count < 3 {
                pts[count] = (s.x as f32, s.y as f32);
            }
            count += 1;
        }
    }

    if count != 3 {
        tp.swipe_ref = None;
        tp.swiped = false;
    }

    // Pointer motion is one finger's job: two fingers are a gesture, wherever that gesture
    // goes, so they do not drag the cursor along with them. Except when the caller says
    // otherwise or the pad is held down, when every finger is pointing, because a
    // click-drag needs one contact on the button and another to move with.
    let pointer_only = tp.btn_down;
    if count == 1 || (pointer_only && count >= 1) {
        // Latch onto the original finger's slot and keep it until that finger
        // lifts; extra fingers are ignored. Adopt a new primary only once the
        // current one is up.
        // Keep the finger that is already steering, unless it lifted or has since
        // parked. Otherwise take the earliest one that is not parked.
        let primary = match tp.primary_slot {
            Some(s)
                if tp.slots[s].active
                    && !tp.slots[s].resting
                    && !tp.slots[s].faint
                    && !(tp.btn_down && in_button_strip(tp, s, set)) =>
            {
                Some(s)
            }
            _ => pointer_slot(tp, set),
        };
        if primary != tp.primary_slot {
            if tp.debug_jumps {
                let at = |o: Option<usize>| match o {
                    Some(i) => format!("slot {i} at {},{}", tp.slots[i].x, tp.slots[i].y),
                    None => "none".to_string(),
                };
                println!(
                    "om_wm: pointer primary {} -> {} (count {count}, btn {})",
                    at(tp.primary_slot),
                    at(primary),
                    tp.btn_down
                );
            }
            tp.primary_slot = primary;
            tp.prev_single = None;
            // A different finger is driving: park until it has moved deliberately.
            tp.ptr_ref = None;
            tp.ptr_armed = false;
        }
        if let Some(p) = primary {
            let (fx, fy) = (tp.slots[p].x as f32, tp.slots[p].y as f32);
            if tp.ptr_ref.is_none() {
                tp.ptr_ref = Some((fx, fy));
            }
            // Park the cursor until the finger has travelled far enough from where it
            // landed, or was last pressed, to count as a move rather than the wobble of
            // a finger settling or pushing the pad down. The freeze covers the press
            // itself, where the travel can be larger than any sane threshold.
            if !tp.ptr_armed {
                let frozen = tp.now - tp.press_time < set.press_freeze_secs;
                if let Some((rx, ry)) = tp.ptr_ref {
                    let moved = ((fx - rx).powi(2) + (fy - ry).powi(2)).sqrt();
                    if !frozen && moved >= tp.span * set.pointer_start_frac {
                        tp.ptr_armed = true;
                        // Start from here, so arming does not jump by the threshold.
                        tp.prev_single = Some((fx, fy));
                    }
                }
            }
            if tp.ptr_armed {
                if let (Some((px, py)), Some(cur)) = (tp.prev_single, cursor) {
                    tp.ptr_accum_x += (fx - px) * set.pointer_sens;
                    tp.ptr_accum_y += (fy - py) * set.pointer_sens;
                    let idx = tp.ptr_accum_x.trunc();
                    let idy = tp.ptr_accum_y.trunc();
                    tp.ptr_accum_x -= idx;
                    tp.ptr_accum_y -= idy;
                    // A frame that moves the cursor this far is not a finger moving, it is a
                    // baseline that belongs to a different contact than the position being
                    // measured. Report everything that went into it.
                    const JUMP_PX: f32 = 30.0;
                    if tp.debug_jumps && (idx.abs() >= JUMP_PX || idy.abs() >= JUMP_PX) {
                        println!(
                            "om_wm: pointer jump {idx},{idy} from slot {p} \
                             finger {fx},{fy} prev {px},{py} count {count} \
                             ids {:?} active {:?} resting {:?} faint {:?}",
                            tp.slots.iter().map(|s| s.id).collect::<Vec<_>>(),
                            tp.slots.iter().map(|s| s.active).collect::<Vec<_>>(),
                            tp.slots.iter().map(|s| s.resting).collect::<Vec<_>>(),
                            tp.slots.iter().map(|s| s.faint).collect::<Vec<_>>(),
                        );
                    }
                    cursor::move_by(cur, idx as i32, idy as i32);
                }
                tp.prev_single = Some((fx, fy));
            }
        }
        tp.prev_centroid = None;
        tp.prev_dist = None;
        tp.mode = GestureMode::None;
        return out;
    }
    tp.prev_single = None;
    tp.primary_slot = None;

    // Holding the pad, or Super: this is a drag, so nothing here becomes a scroll. The
    // baselines are still kept current so that letting go does not start the next gesture
    // with a jump.
    if pointer_only {
        tp.prev_centroid = None;
        tp.prev_dist = None;
        tp.mode = GestureMode::None;
        tp.pan_ref = None;
        tp.pan_armed = false;
        tp.zoom_ref = None;
        tp.zoom_armed = false;
        return out;
    }

    // Three fingers say one thing: which way they went. It fires once, on the frame the
    // travel passes the threshold, and they are ignored from then until they lift, so a hand
    // that carries on drifting cannot fire it twice, and cannot fire the other way either by
    // coming back. Nothing here pans or zooms; the caller decides what a swipe means.
    if count == 3 {
        let centroid = (
            (pts[0].0 + pts[1].0 + pts[2].0) / 3.0,
            (pts[0].1 + pts[1].1 + pts[2].1) / 3.0,
        );
        match tp.swipe_ref {
            None => tp.swipe_ref = Some(centroid),
            Some(r) if !tp.swiped => {
                // Up is y decreasing, the same direction the scroll convention calls
                // positive. Whichever way it went has to lead the sideways travel, or a
                // diagonal drag across the pad would count as one.
                let up = r.1 - centroid.1;
                let across = (centroid.0 - r.0).abs();
                if up.abs() >= tp.span * set.swipe_frac && up.abs() > across {
                    tp.swiped = true;
                    if up > 0.0 {
                        out.swipe_up = true;
                    } else {
                        out.swipe_down = true;
                    }
                }
            }
            _ => {}
        }
    }

    // Two fingers, and only two. Anything else is not a gesture: nothing to pan or zoom, and
    // panning stops dead when the fingers lift, no glide.
    if count != 2 {
        tp.prev_centroid = None;
        tp.prev_dist = None;
        tp.mode = GestureMode::None;
        tp.pan_ref = None;
        tp.pan_armed = false;
        tp.zoom_ref = None;
        tp.zoom_armed = false;
        return out;
    }

    let centroid = ((pts[0].0 + pts[1].0) * 0.5, (pts[0].1 + pts[1].1) * 0.5);
    let dx = pts[0].0 - pts[1].0;
    let dy = pts[0].1 - pts[1].1;
    let dist = (dx * dx + dy * dy).sqrt();

    // Pan and zoom each have to travel a minimum distance from where the gesture
    // began before they do anything: a tap, a resting hand or a hand
    // settling on the pad should move neither. Arming re-baselines, so the motion
    // starts from there rather than jumping by the whole threshold. The pan
    // threshold is also the drift a tap is allowed, so a contact either pans or
    // stays eligible as a tap.
    let move_start = tp.span * set.move_start_frac;
    let pinch_start = tp.span * set.pinch_start_frac;
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
        // Panning, and the pinch has not been armed: the reference creeps toward the fingers,
        // so their drift never adds up to a pinch nobody meant.
        Some(r) if tp.mode == GestureMode::Pan => {
            tp.zoom_ref = Some(r + (dist - r) * set.zoom_ref_follow);
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
            let mode_eps = tp.span * set.mode_eps_frac;
            let want_zoom = tp.zoom_armed
                && zoom_delta > mode_eps
                && zoom_delta > pan_delta * set.mode_bias;
            let want_pan = tp.pan_armed
                && pan_delta > mode_eps
                && pan_delta > zoom_delta * set.mode_bias;
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
                    // In the scroll convention: positive up and right. Content follows the
                    // fingers, which is why a finger moving down scrolls up.
                    out.scroll_x = (centroid.0 - pc.0) * set.pan_sens;
                    out.scroll_y = -(centroid.1 - pc.1) * set.pan_sens;
                    tp.last_pan = (out.scroll_x, out.scroll_y);
                    tp.last_zoom = 1.0;
                }
                GestureMode::Zoom => {
                    let deadzone = tp.span * set.pinch_deadzone_frac;
                    let ddist = dist - pd;
                    if ddist.abs() > deadzone {
                        let beyond = ddist - ddist.signum() * deadzone;
                        let ratio = (pd + beyond) / pd;
                        out.pinch = ratio;
                        tp.last_zoom = ratio;
                        tp.last_pan = (0.0, 0.0);
                    }
                }
                GestureMode::None => {
                    tp.last_pan = (0.0, 0.0);
                    tp.last_zoom = 1.0;
                }
            }
        }
        _ => {
            // Gesture start: no baselines yet, so nothing to apply this frame.
            tp.mode = GestureMode::None;
        }
    }

    tp.prev_centroid = Some(centroid);
    tp.prev_dist = Some(dist);
    out
}
