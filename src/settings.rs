//
// Settings (Data Oriented zone)
//
// The feel of the thing: trackpad sensitivities, gesture thresholds, camera rates.
// These were consts, which meant a rebuild to answer "is 0.12 better than 0.09", and
// feel is the one thing that cannot be reasoned about, only tried. So they are data
// now, read from a file, and the file is re-read while om_wm runs.
//
// One flat key = value per line, parsed here in a few dozen lines. No format library:
// a flat set of numbers does not need nesting, and a parser we own cannot pull in a
// dependency tree or fail in ways we cannot read.
//
// Unknown keys are reported and ignored rather than fatal, so a file from a newer or
// older build still loads. Same for a value that will not parse: the default stands and
// says so. A bad edit therefore costs you one line of behaviour, never the session.
//
// Every value here is read where it is used, per event or per frame, so a reload lands
// immediately. trackpad_mode is the exception and says so: the device has to be
// reopened for it, which happens at startup.
//

use std::path::Path;
use std::time::SystemTime;

use crate::input::TrackpadMode;
use crate::render::DmabufMode;

//
// Constants
//

// Where the file lives, unless OM_WM_CONF says otherwise.
pub const DEFAULT_PATH: &str = "om_wm.conf";

//
// Types
//

#[derive(Clone, Copy)]
pub struct Settings {
    // Which code drives the trackpad. Applied when the device is opened, so a reload
    // does not switch it.
    pub trackpad_mode: TrackpadMode,

    // Trackpad: pointer.
    // Canvas pixels of cursor travel per device unit of finger travel.
    pub pointer_sens: f32,
    // How far a finger must travel from where it landed, or from a click, before it
    // moves the cursor at all. A fraction of the pad's shorter axis.
    pub pointer_start_frac: f32,
    // How long the cursor stays parked either side of a click, in seconds.
    pub press_freeze_secs: f64,
    // How long every finger may stay on the pad and still count as a tap rather than a gesture,
    // in seconds. How far they may travel is move_start_frac, which a tap shares with the pan
    // threshold: a contact either pans or stays eligible as a tap, never both.
    //
    // Only the three-finger double tap uses this today, which is "bring me the window under the
    // cursor". A tap is a deliberate, quick thing, so this wants to be short: too long and a hand
    // that lands, hesitates and lifts starts moving your view around.
    pub tap_max_secs: f64,
    // And how far its fingers may drift and still be a tap, as a fraction of the pad.
    //
    // Its own allowance rather than move_start_frac, which is a two-finger pan threshold and far
    // too tight for three fingers: measured on this pad, real three-finger taps drift 35 to 316
    // units while swipes start around 960, so the two are separated by a factor of seven and the
    // pan's 81 sat in the middle of the taps. A hand landing three fingers rolls the outer two,
    // and the worst of the three is what this is measured against.
    pub tap_move_frac: f32,

    // Trackpad: two-finger gestures.
    // Canvas units panned per device unit of centroid travel.
    pub pan_sens: f32,
    // And a multiplier on top of that for scroll that goes into a window instead of the
    // canvas. Two-finger scroll is one gesture with two destinations, and the right speed
    // for dragging a canvas around is not the right speed for scrolling a page: the canvas
    // moves under your fingers one to one, while a page wants to move further than they
    // do. 1.0 sends a window exactly what the canvas would have panned.
    pub window_scroll_sens: f32,
    // How much the minor axis of a finger scroll has to be doing, relative to the major one,
    // before a window is told about it. Fingers travelling up a pad wander sideways, and a
    // client that resolves one axis at a time can end up reading the wander instead of the
    // scroll. Below this ratio the minor axis is held back and only the major one is sent;
    // above it both go, so a genuinely diagonal gesture still scrolls a map or a wide page in
    // two dimensions. Measured over the recent part of the gesture, not the whole of it, so
    // turning a corner without lifting your fingers opens it up. 0 sends both axes always,
    // which is what we did before this existed. The canvas is never locked either way.
    pub scroll_axis_lock_frac: f32,
    // Travel before a pan or a pinch does anything, as a fraction of the pad. The pan
    // threshold doubles as the drift a tap is allowed.
    pub move_start_frac: f32,
    pub pinch_start_frac: f32,
    // How far three fingers must travel, as a fraction of the pad, to count as a swipe.
    // Larger than a pan threshold on purpose: a swipe is a command rather than a motion being
    // followed, and firing one by accident costs you your place on the canvas.
    pub swipe_frac: f32,
    // How much one motion has to lead the other to take over pan/zoom, and the noise
    // floor below which neither counts.
    pub mode_bias: f32,
    pub mode_eps_frac: f32,
    // Pinch motion ignored around the current distance, to stop a pan wobbling the zoom.
    pub pinch_deadzone_frac: f32,
    // How fast the distance a pinch is measured against creeps toward where the fingers
    // actually are, while they are panning, as a fraction of the gap per frame.
    //
    // Two fingers dragged across a pad drift apart on their own, and a reference fixed where
    // they landed eventually reads that drift as a pinch: the zoom arms, some frame where the
    // separation leads the travel takes the gesture, and a pan has zoomed a little. Letting
    // the reference creep forgives drift, because it keeps moving to meet it, while a real
    // pinch outruns it and still counts.
    //
    // 0 never creeps, which is what om_wm did before this existed. 0.05 closes half the drift
    // in about a fifth of a second. Higher forgives more and demands a brisker pinch.
    pub zoom_ref_follow: f32,
    // Where a three-finger swipe turns the view around: the middle of the screen, or the
    // cursor.
    //
    // False, the middle of the screen, is the default because a swipe moves the whole canvas
    // rather than a piece of it. It is "show me everything" and "put me back", and where the
    // cursor happens to be resting is not what the gesture is about; anchoring there sends the
    // view somewhere your hands did not choose.
    //
    // True anchors on the cursor instead, which keeps whatever you are pointing at exactly
    // where it is while the scale changes around it. That is the pinch's rule, and it makes
    // the swipe a faster way of doing the same thing rather than a different kind of move.
    pub swipe_zoom_at_cursor: bool,

    // Trackpad: how much contact counts as a touch at all, in the raw units the overlay
    // shows (this pad reports 0..2048). Below the first threshold a contact is ignored
    // entirely; once counted it is only dropped below the second, which stops a contact
    // hovering at the boundary from flickering in and out. Zero for either disables it,
    // which is the default: the right number is a property of the hardware and the hand,
    // and has to be read off the overlay rather than guessed.
    pub touch_min_size: f32,
    pub touch_drop_size: f32,

    // Trackpad: resting fingers. A hand rests on the pad, so a finger that sits in the
    // bottom of it and stops moving is parked: it does not steer the cursor, does not
    // count toward a two-finger gesture, and does not decide which button a click is.
    // The zone is measured from the bottom edge, idleness in seconds, and the movement
    // that counts as not-idle as a fraction of the pad.
    pub rest_zone_frac: f32,
    pub rest_secs: f64,
    pub rest_move_frac: f32,

    // Trackpad: software buttons on a clickpad. The strip is measured from the bottom
    // edge, the split across it, both as fractions.
    pub button_strip: f32,
    pub button_split: f32,

    // Mouse.
    // Mac-natural scrolling: content follows the fingers. Also decides the sign clients
    // are sent, so the canvas and the app inside a window always agree.
    pub invert_scroll: bool,
    // Canvas pixels panned per horizontal wheel notch, at zoom 1.
    pub hwheel_pan: f32,
    // Pixels a client is told to scroll per wheel notch, alongside the discrete step.
    pub wheel_step_px: f32,
    // Gap within which a second middle click counts as a double click, milliseconds.
    pub double_click_ms: u32,

    // Debugging, as settings rather than as environment variables, so they can be turned on
    // while om_wm is running and turned off again without restarting it. What they report is our
    // own reading of the pad, which is the thing worth seeing: raw events say what the kernel
    // sent, these say what we made of it and why.
    //
    // debug_taps reports every contact and whether it counted as a tap. debug_jumps reports any
    // frame that moves the cursor further than a hand could have, with every slot's state.
    pub debug_taps: bool,
    pub debug_jumps: bool,

    // Windows.
    // Smallest a Super+right-drag will ask a window to be, in canvas units, for clients
    // that never declared a minimum of their own.
    pub resize_min_px: f32,
    // Whether a resize drag stretches the window locally while it waits for the client.
    //
    // Off by default, which is what every other Wayland compositor does: the window is
    // whatever size the client last committed, and the corner reaches the cursor when the
    // client says it has. Nothing on screen is ever a size the window is not, so there is
    // no overshoot to take back and no snap when the drag ends.
    //
    // On, the quad is scaled to the cursor while the client catches up. The corner tracks
    // your hand exactly, at the cost of showing content at a size it was not drawn at, and
    // of having to undo the difference whenever the client declines to follow.
    pub resize_stretch: bool,
    // During a resize drag the quad already shows the size you are dragging to, so this
    // only paces how often the client is asked to re-render into it: the next ask waits
    // for the client to answer, or for this many frames, whichever comes first. Lower
    // means fresher content from a client that ignores configures, at the cost of asking
    // one that is merely slow to redraw work it will throw away.
    pub resize_wait_frames: u32,
    // Whether to draw what a client padded around its window, which is where its shadow and
    // its rounded corners live.
    //
    // A toolkit commits a surface larger than the geometry it declares, and we cannot ask it
    // not to: server-side decorations are not implemented by the toolkits that do this. So the
    // choice is only whether to show what is already in the buffer. Cropping to the geometry
    // slices the shadow off square and fills the rounded corners back in, which is what it
    // looked like before this existed. Drawing it costs the padding's worth of transparent
    // pixels per window and lets a shadow fall across whatever is behind it.
    //
    // Only the drawing. Hit tests, placement, drags and maximizing all still work in the
    // geometry rectangle, so the padding can never be clicked.
    pub draw_shadows: bool,

    // What happens to a client's dmabuf once we have imported it: hold it and sample it in
    // place, or copy it into a texture of our own and hand it back. See DmabufMode.
    pub dmabuf_mode: DmabufMode,

    // Camera.
    // Keyboard zoom rate, for Super with plus or minus.
    pub zoom_rate_per_sec: f32,
    // Zoom limits, and the scale the resets return to.
    pub zoom_min: f32,
    pub zoom_max: f32,
    pub zoom_default: f32,
    // The other scale, and the boundary between the two ways of working.
    //
    // The zoom only ever comes to rest at this or at zoom_default, so which one it rests at
    // is the mode. Out here the canvas owns everything: no window takes focus, hears the
    // pointer or receives a key, and two fingers pan and pinch without asking Super. In there
    // the windows are applications again.
    //
    // Where a three-finger swipe up takes the view, and the far end of what Super+Escape
    // toggles between. Nothing else settles here: a pinch is left wherever it is let go.
    pub overview_zoom: f32,
    // Perspective field of view in degrees. Larger means more parallax on a lifted
    // window.
    pub fov_deg: f32,
    // How fast the view travels when a gesture sends it somewhere, as a fraction of the
    // distance remaining per second. Only the swipes and Super+Escape send it anywhere; a
    // pinch is never followed by a journey. 0 makes them jump instead of travel.
    pub zoom_ease_rate: f32,
    // How close to zoom_default the zoom may sit and still be taken the rest of the way, as a
    // difference in scale. A zoom of 0.98 or 1.0004 costs every window on screen a resample
    // and shows nothing for it: the pixel grid disengages, text softens, and nothing was
    // aiming there. Inside this band the zoom eases to zoom_default, around wherever the
    // pointer is, so nothing slides. 0 switches it off and the zoom is left exactly where it
    // was put, which is what the canvas did before this existed.
    //
    // Tested every frame, whatever moved the zoom: a pinch, the wheel, the keyboard, an ease
    // that was interrupted. Fingers actively pinching are the exception, since the band is
    // theirs to move through while they are asking.
    //
    // This is a detent, not a mode: it only ever pulls to zoom_default and only from within
    // this band. Everywhere else the zoom is still a place you are.
    pub zoom_detent: f32,
    // How much of each axis the corner a resize started on keeps, while that resize lasts.
    //
    // A drag can change which corner it holds without letting go, decided by which side of
    // the window the cursor is on. Splitting the window down the middle makes that too easy
    // to trigger by accident: shrinking inward walks the cursor toward the middle, and
    // crossing it hands the drag to the opposite corner just as you are getting somewhere.
    // So the corner you grabbed keeps this much of each axis and the boundary moves out of
    // its way. 0.5 is an even split and no bias at all; 0.75 gives the starting quadrant
    // three quarters of the window in both directions.
    pub resize_corner_hold: f32,
}

// What the file is compared against: the values that ship, and the mtime that was last
// loaded.
pub struct Watch {
    path: String,
    mtime: Option<SystemTime>,
}

//
// Defaults
//

pub fn defaults() -> Settings {
    Settings {
        trackpad_mode: TrackpadMode::Custom,

        pointer_sens: 0.25,
        pointer_start_frac: 0.008,
        press_freeze_secs: 0.12,
        tap_max_secs: 0.25,
        tap_move_frac: 0.06,

        pan_sens: 0.12,
        window_scroll_sens: 1.0,
        scroll_axis_lock_frac: 0.5,
        move_start_frac: 0.012,
        pinch_start_frac: 0.02,
        swipe_frac: 0.15,
        mode_bias: 1.6,
        mode_eps_frac: 0.0015,
        pinch_deadzone_frac: 0.0005,
        zoom_ref_follow: 0.05,
        swipe_zoom_at_cursor: false,


        touch_min_size: 0.0,
        touch_drop_size: 0.0,

        rest_zone_frac: 0.4,
        rest_secs: 0.5,
        rest_move_frac: 0.004,

        button_strip: 1.0 / 3.0,
        button_split: 0.5,

        invert_scroll: true,
        hwheel_pan: 60.0,
        wheel_step_px: 15.0,
        double_click_ms: 400,

        debug_taps: false,
        debug_jumps: false,
        resize_min_px: 120.0,
        resize_stretch: false,
        resize_wait_frames: 8,
        draw_shadows: true,
        dmabuf_mode: DmabufMode::Hold,

        zoom_rate_per_sec: 2.0,
        zoom_min: 0.1,
        zoom_max: 8.0,
        zoom_default: 1.0,
        overview_zoom: 0.57,
        fov_deg: 40.0,
        zoom_ease_rate: 12.0,
        zoom_detent: 0.05,
        resize_corner_hold: 0.75,
    }
}

// The signs a client is sent for scroll, derived rather than stored, and different per
// axis because the two conventions disagree about what the axes mean.
//
// Ours is positive up and right, the direction the fingers or the wheel moved. Wayland's
// is positive vertical for scrolling down, which moves content up, and positive
// horizontal for scrolling right, which moves content left. So content-follows-fingers,
// which is what inverted (Mac-natural) scrolling means, needs a positive vertical for
// fingers moving up and a negative horizontal for fingers moving right.
//
// Hence the horizontal sign is always the opposite of the vertical one. Getting this
// wrong is invisible until you scroll sideways in a browser and the page goes the wrong
// way while the canvas underneath it goes the right way.
pub fn client_scroll_sign(set: &Settings) -> f32 {
    if set.invert_scroll {
        1.0
    } else {
        -1.0
    }
}

pub fn client_hscroll_sign(set: &Settings) -> f32 {
    -client_scroll_sign(set)
}

//
// Load
//

// Read the file over a set of defaults. A missing file is not an error: it means run
// with what shipped.
pub fn load(path: &str) -> Settings {
    let mut set = defaults();
    let Ok(text) = std::fs::read_to_string(path) else {
        println!("om_wm: settings: no {path}, using defaults");
        return set;
    };
    let mut applied = 0usize;
    for (n, raw) in text.lines().enumerate() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            eprintln!("om_wm: settings: {path}:{} is not key = value", n + 1);
            continue;
        };
        if apply(&mut set, key.trim(), value.trim(), path, n + 1) {
            applied += 1;
        }
    }
    println!("om_wm: settings: {applied} from {path}");
    set
}

// One key. Returns whether it landed, so load can say how much of the file was used.
fn apply(set: &mut Settings, key: &str, value: &str, path: &str, line: usize) -> bool {
    // Each arm reads the value in the type that key wants, and leaves the default in
    // place if it will not parse.
    macro_rules! f32_key {
        ($field:ident) => {{
            match value.parse::<f32>() {
                Ok(v) => {
                    set.$field = v;
                    return true;
                }
                Err(_) => {
                    eprintln!("om_wm: settings: {path}:{line}: {key} wants a number, got '{value}'");
                    return false;
                }
            }
        }};
    }
    macro_rules! f64_key {
        ($field:ident) => {{
            match value.parse::<f64>() {
                Ok(v) => {
                    set.$field = v;
                    return true;
                }
                Err(_) => {
                    eprintln!("om_wm: settings: {path}:{line}: {key} wants a number, got '{value}'");
                    return false;
                }
            }
        }};
    }

    match key {
        "trackpad_mode" => match value {
            "custom" => set.trackpad_mode = TrackpadMode::Custom,
            "libinput" => set.trackpad_mode = TrackpadMode::Libinput,
            _ => {
                eprintln!(
                    "om_wm: settings: {path}:{line}: trackpad_mode wants custom or libinput"
                );
                return false;
            }
        },
        "pointer_sens" => f32_key!(pointer_sens),
        "pointer_start_frac" => f32_key!(pointer_start_frac),
        "press_freeze_secs" => f64_key!(press_freeze_secs),
        "tap_max_secs" => f64_key!(tap_max_secs),
        "tap_move_frac" => f32_key!(tap_move_frac),
        "pan_sens" => f32_key!(pan_sens),
        "window_scroll_sens" => f32_key!(window_scroll_sens),
        "scroll_axis_lock_frac" => f32_key!(scroll_axis_lock_frac),
        "move_start_frac" => f32_key!(move_start_frac),
        "pinch_start_frac" => f32_key!(pinch_start_frac),
        "swipe_frac" => f32_key!(swipe_frac),
        "mode_bias" => f32_key!(mode_bias),
        "mode_eps_frac" => f32_key!(mode_eps_frac),
        "pinch_deadzone_frac" => f32_key!(pinch_deadzone_frac),
        "zoom_ref_follow" => f32_key!(zoom_ref_follow),
        "touch_min_size" => f32_key!(touch_min_size),
        "touch_drop_size" => f32_key!(touch_drop_size),
        "rest_zone_frac" => f32_key!(rest_zone_frac),
        "rest_secs" => f64_key!(rest_secs),
        "rest_move_frac" => f32_key!(rest_move_frac),
        "button_strip" => f32_key!(button_strip),
        "button_split" => f32_key!(button_split),
        "hwheel_pan" => f32_key!(hwheel_pan),
        "wheel_step_px" => f32_key!(wheel_step_px),
        "resize_min_px" => f32_key!(resize_min_px),
        "resize_stretch" => match value {
            "true" | "1" | "yes" => set.resize_stretch = true,
            "false" | "0" | "no" => set.resize_stretch = false,
            _ => {
                eprintln!("om_wm: settings: {path}:{line}: resize_stretch wants true or false");
                return false;
            }
        },
        "resize_wait_frames" => match value.parse::<u32>() {
            Ok(v) => set.resize_wait_frames = v.max(1),
            Err(_) => {
                eprintln!("om_wm: settings: {path}:{line}: resize_wait_frames wants a whole number");
                return false;
            }
        },
        "dmabuf_mode" => match value {
            "hold" => set.dmabuf_mode = DmabufMode::Hold,
            "blit" => set.dmabuf_mode = DmabufMode::Blit,
            _ => {
                eprintln!(
                    "om_wm: settings: {path}:{line}: dmabuf_mode wants hold or blit"
                );
                return false;
            }
        },
        "zoom_rate_per_sec" => f32_key!(zoom_rate_per_sec),
        "zoom_min" => f32_key!(zoom_min),
        "zoom_max" => f32_key!(zoom_max),
        "zoom_default" => f32_key!(zoom_default),
        "overview_zoom" => f32_key!(overview_zoom),
        "fov_deg" => f32_key!(fov_deg),
        "zoom_ease_rate" => f32_key!(zoom_ease_rate),
        "zoom_detent" => f32_key!(zoom_detent),
        "resize_corner_hold" => f32_key!(resize_corner_hold),
        "swipe_zoom_at_cursor" => match value {
            "true" | "1" | "yes" => set.swipe_zoom_at_cursor = true,
            "false" | "0" | "no" => set.swipe_zoom_at_cursor = false,
            _ => {
                eprintln!(
                    "om_wm: settings: {path}:{line}: swipe_zoom_at_cursor wants true or false"
                );
                return false;
            }
        },
        "draw_shadows" => match value {
            "true" | "1" | "yes" => set.draw_shadows = true,
            "false" | "0" | "no" => set.draw_shadows = false,
            _ => {
                eprintln!("om_wm: settings: {path}:{line}: draw_shadows wants true or false");
                return false;
            }
        },
        "debug_taps" => match value {
            "true" | "1" | "yes" => set.debug_taps = true,
            "false" | "0" | "no" => set.debug_taps = false,
            _ => {
                eprintln!("om_wm: settings: {path}:{line}: debug_taps wants true or false");
                return false;
            }
        },
        "debug_jumps" => match value {
            "true" | "1" | "yes" => set.debug_jumps = true,
            "false" | "0" | "no" => set.debug_jumps = false,
            _ => {
                eprintln!("om_wm: settings: {path}:{line}: debug_jumps wants true or false");
                return false;
            }
        },
        "invert_scroll" => match value {
            "true" | "1" | "yes" => set.invert_scroll = true,
            "false" | "0" | "no" => set.invert_scroll = false,
            _ => {
                eprintln!("om_wm: settings: {path}:{line}: invert_scroll wants true or false");
                return false;
            }
        },
        "double_click_ms" => match value.parse::<u32>() {
            Ok(v) => set.double_click_ms = v,
            Err(_) => {
                eprintln!("om_wm: settings: {path}:{line}: double_click_ms wants a whole number");
                return false;
            }
        },
        _ => {
            eprintln!("om_wm: settings: {path}:{line}: unknown key '{key}'");
            return false;
        }
    }
    true
}

// Values that would break the code that reads them, rather than merely feel wrong.
// Clamped rather than rejected: a file with one silly number should still load, and a
// zoom range of zero would divide by it.
fn sanitise(set: &mut Settings) {
    set.zoom_min = set.zoom_min.max(0.001);
    set.zoom_max = set.zoom_max.max(set.zoom_min * 1.001);
    set.zoom_default = set.zoom_default.clamp(set.zoom_min, set.zoom_max);
    set.zoom_detent = set.zoom_detent.max(0.0);
    set.zoom_ref_follow = set.zoom_ref_follow.clamp(0.0, 1.0);
    set.fov_deg = set.fov_deg.clamp(1.0, 170.0);
    set.button_strip = set.button_strip.clamp(0.0, 1.0);
    set.touch_min_size = set.touch_min_size.max(0.0);
    // A drop threshold at or above the entry one would defeat the hysteresis, so fall
    // back to a fraction of it rather than pretending to honour a bad pair.
    set.touch_drop_size = if set.touch_drop_size > 0.0 && set.touch_drop_size < set.touch_min_size {
        set.touch_drop_size
    } else {
        set.touch_min_size * 0.7
    };
    set.rest_zone_frac = set.rest_zone_frac.clamp(0.0, 1.0);
    set.rest_secs = set.rest_secs.max(0.0);
    set.rest_move_frac = set.rest_move_frac.max(0.0);
    set.resize_min_px = set.resize_min_px.max(1.0);
    set.button_split = set.button_split.clamp(0.0, 1.0);
    set.pointer_start_frac = set.pointer_start_frac.max(0.0);
    set.window_scroll_sens = set.window_scroll_sens.max(0.0);
    set.scroll_axis_lock_frac = set.scroll_axis_lock_frac.max(0.0);
    set.press_freeze_secs = set.press_freeze_secs.max(0.0);
    set.tap_max_secs = set.tap_max_secs.max(0.0);
    set.tap_move_frac = set.tap_move_frac.max(0.0);
}

//
// Reload
//

pub fn watch_new(path: &str) -> Watch {
    Watch { path: path.to_string(), mtime: mtime_of(path) }
}

// Re-read the file if it has been written since the last look. Called on a frame
// interval rather than every frame: one stat a second is free, and inotify would be
// another fd and another failure mode for something a human triggers by saving a file.
pub fn reload_if_changed(watch: &mut Watch, set: &mut Settings) -> bool {
    let now = mtime_of(&watch.path);
    if now == watch.mtime {
        return false;
    }
    watch.mtime = now;
    let mode = set.trackpad_mode;
    *set = load(&watch.path);
    // Reopening the device is not something a file save should do, so the mode in force
    // stays in force until the next start.
    if set.trackpad_mode != mode {
        eprintln!("om_wm: settings: trackpad_mode change takes effect on restart");
        set.trackpad_mode = mode;
    }
    sanitise(set);
    true
}

// Also reload on demand, for when you want to know that it took.
pub fn reload(watch: &mut Watch, set: &mut Settings) {
    watch.mtime = mtime_of(&watch.path);
    let mode = set.trackpad_mode;
    *set = load(&watch.path);
    set.trackpad_mode = mode;
    sanitise(set);
}

fn mtime_of(path: &str) -> Option<SystemTime> {
    std::fs::metadata(Path::new(path)).ok()?.modified().ok()
}

// The first load, including the sanity pass the reloads get.
pub fn init(path: &str) -> Settings {
    let mut set = load(path);
    sanitise(&mut set);
    set
}
