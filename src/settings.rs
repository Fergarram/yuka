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

    // Trackpad: two-finger gestures.
    // Canvas units panned per device unit of centroid travel.
    pub pan_sens: f32,
    // And a multiplier on top of that for scroll that goes into a window instead of the
    // canvas. Two-finger scroll is one gesture with two destinations, and the right speed
    // for dragging a canvas around is not the right speed for scrolling a page: the canvas
    // moves under your fingers one to one, while a page wants to move further than they
    // do. 1.0 sends a window exactly what the canvas would have panned.
    pub window_scroll_sens: f32,
    // Travel before a pan or a pinch does anything, as a fraction of the pad. The pan
    // threshold doubles as the drift a tap is allowed.
    pub move_start_frac: f32,
    pub pinch_start_frac: f32,
    // How much one motion has to lead the other to take over pan/zoom, and the noise
    // floor below which neither counts.
    pub mode_bias: f32,
    pub mode_eps_frac: f32,
    // Pinch motion ignored around the current distance, to stop a pan wobbling the zoom.
    pub pinch_deadzone_frac: f32,

    // Trackpad: taps.
    // Longest a contact can last and still count as a tap, and the window in which a
    // second tap makes it a double.
    pub tap_max_secs: f64,
    pub double_tap_secs: f64,

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

    // Windows.
    // Smallest a Super+right-drag will ask a window to be, in canvas units, for clients
    // that never declared a minimum of their own.
    pub resize_min_px: f32,
    // Whether a resize drag stretches the window locally while it waits for the client.
    // On, the corner tracks the cursor exactly and the contents are scaled until the
    // client's own render catches up. Off, nothing moves until the client answers, which
    // is what every other Wayland compositor does, and what makes a slow client feel like
    // the window is trailing your hand.
    pub resize_stretch: bool,
    // During a resize drag the quad already shows the size you are dragging to, so this
    // only paces how often the client is asked to re-render into it: the next ask waits
    // for the client to answer, or for this many frames, whichever comes first. Lower
    // means fresher content from a client that ignores configures, at the cost of asking
    // one that is merely slow to redraw work it will throw away.
    pub resize_wait_frames: u32,
    // What happens to a client's dmabuf once we have imported it: hold it and sample it in
    // place, hand it straight back and tear, or copy it into a texture of our own and hand it
    // back. See DmabufMode.
    pub dmabuf_mode: DmabufMode,

    // Camera.
    // Keyboard pan speed in screen pixels per second, and keyboard zoom rate.
    pub pan_px_per_sec: f32,
    pub zoom_rate_per_sec: f32,
    // Zoom limits, and the scale the resets return to.
    pub zoom_min: f32,
    pub zoom_max: f32,
    pub zoom_default: f32,
    // Perspective field of view in degrees. Larger means more parallax on a lifted
    // window.
    pub fov_deg: f32,
    // Where a pinch that zoomed out settles when the fingers lift. Above this the view
    // springs back to 1:1; at it or below, the zoom is taken as meant and left alone. 1:1 is
    // where a window is sampled texel for texel, so drifting a little off it costs sharpness
    // for nothing, while a deliberate zoom out is somewhere you asked to be.
    pub zoom_spring_floor: f32,
    // How fast it travels back, as a fraction of the remaining distance per second. 0 turns
    // the spring off and a released pinch stays exactly where it was let go.
    pub zoom_spring_rate: f32,
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

        pan_sens: 0.12,
        window_scroll_sens: 1.0,
        move_start_frac: 0.012,
        pinch_start_frac: 0.02,
        mode_bias: 1.6,
        mode_eps_frac: 0.0015,
        pinch_deadzone_frac: 0.0005,

        tap_max_secs: 0.25,
        double_tap_secs: 0.4,

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

        resize_min_px: 120.0,
        resize_stretch: true,
        resize_wait_frames: 8,
        dmabuf_mode: DmabufMode::Hold,

        pan_px_per_sec: 900.0,
        zoom_rate_per_sec: 2.0,
        zoom_min: 0.1,
        zoom_max: 8.0,
        zoom_default: 1.0,
        fov_deg: 40.0,
        zoom_spring_floor: 0.75,
        zoom_spring_rate: 12.0,
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
        "pan_sens" => f32_key!(pan_sens),
        "window_scroll_sens" => f32_key!(window_scroll_sens),
        "move_start_frac" => f32_key!(move_start_frac),
        "pinch_start_frac" => f32_key!(pinch_start_frac),
        "mode_bias" => f32_key!(mode_bias),
        "mode_eps_frac" => f32_key!(mode_eps_frac),
        "pinch_deadzone_frac" => f32_key!(pinch_deadzone_frac),
        "tap_max_secs" => f64_key!(tap_max_secs),
        "double_tap_secs" => f64_key!(double_tap_secs),
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
            "release" => set.dmabuf_mode = DmabufMode::Release,
            "blit" => set.dmabuf_mode = DmabufMode::Blit,
            _ => {
                eprintln!(
                    "om_wm: settings: {path}:{line}: dmabuf_mode wants hold, release or blit"
                );
                return false;
            }
        },
        "pan_px_per_sec" => f32_key!(pan_px_per_sec),
        "zoom_rate_per_sec" => f32_key!(zoom_rate_per_sec),
        "zoom_min" => f32_key!(zoom_min),
        "zoom_max" => f32_key!(zoom_max),
        "zoom_default" => f32_key!(zoom_default),
        "fov_deg" => f32_key!(fov_deg),
        "zoom_spring_floor" => f32_key!(zoom_spring_floor),
        "zoom_spring_rate" => f32_key!(zoom_spring_rate),
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
    set.press_freeze_secs = set.press_freeze_secs.max(0.0);
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
