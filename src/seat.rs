//
// Session and VT ownership (Data Oriented zone)
//
// om_wm runs as a logind session controller, through libseat. Taking control is
// what makes the rest work: logind then owns our VT and sets it up the way every
// compositor needs it (KDSKBMODE K_OFF, KDSETMODE KD_GRAPHICS, VT_SETMODE
// VT_PROCESS), and it restores all of that when we disconnect, even if we crash.
//
// K_OFF is the load bearing part. The kernel discards console keystrokes on our
// VT while still tracking modifiers (kbd_keycode dispatches KT_SHIFT even in
// VC_OFF), and it stops acting on the switch chords (KT_CONS), which are ours to
// implement. That is why we need no exclusive evdev grab: nothing leaks into a
// tty, and no modifier can stick because the kernel never stops seeing releases.
//
// Switching is libseat_switch_session. Activation and deactivation arrive as
// callbacks, so there is nothing to poll: we drop DRM master when the session
// goes away and take it back when it returns.
//
// libseat is loaded with dlopen so it stays a runtime option, not a build
// dependency. Without it (or without a session to control) init returns None and
// main falls back to grabbing input with no VT switching at all.
//
// The card comes from libseat_open_device, and raylib is handed the fd (our patch to
// its DRM platform, see ray::set_drm_fd). That is what makes a switch orderly: logind
// revokes a device it gave us and waits for our acknowledgement before completing the
// switch, so nothing else can be drawing while we still think we own the display. A
// card we opened ourselves behind logind's back gets none of that, which is why
// open_card is preferred and adopt_card is only the fallback for a raylib without the
// patch. The input devices are still opened directly and could follow the same route.
//
// All the dlopen/FFI and raw pointer work is contained here.
//

use std::ffi::{c_char, c_int, c_void, CString};
use std::fs;

use crate::input;

//
// Constants
//

// <drm/drm.h>: DRM_IOCTL_SET_MASTER = _IO('d', 0x1e), DROP_MASTER = _IO('d', 0x1f).
const DRM_IOCTL_SET_MASTER: libc::c_ulong = 0x0000_641e;
const DRM_IOCTL_DROP_MASTER: libc::c_ulong = 0x0000_641f;
// DRM_IOCTL_GET_CAP = _IOWR('d', 0x0c, struct drm_get_cap), asked only to find out
// whether an fd is a KMS node at all.
const DRM_IOCTL_GET_CAP: libc::c_ulong = 0xc010_640c;
const DRM_CAP_DUMB_BUFFER: u64 = 0x1;
// <linux/vt.h>
const VT_GETSTATE: libc::c_ulong = 0x5603;

// Highest VT the vt_stat "in use" bitmask (a u16) can report.
const MAX_VT: u16 = 15;
// How long to wait for libseat to report the session active, in dispatches.
const ENABLE_TRIES: u32 = 100;
const ENABLE_TIMEOUT_MS: c_int = 50;

//
// libseat FFI
//

// Opaque struct libseat.
#[repr(C)]
struct LibSeat {
    _private: [u8; 0],
}

#[repr(C)]
struct SeatListener {
    enable_seat: extern "C" fn(*mut LibSeat, *mut c_void),
    disable_seat: extern "C" fn(*mut LibSeat, *mut c_void),
}

// libseat keeps the listener pointer, so it has to outlive the seat.
static LISTENER: SeatListener =
    SeatListener { enable_seat: on_enable, disable_seat: on_disable };

type OpenSeat = unsafe extern "C" fn(*const SeatListener, *mut c_void) -> *mut LibSeat;
type DisableSeat = unsafe extern "C" fn(*mut LibSeat) -> c_int;
type CloseSeat = unsafe extern "C" fn(*mut LibSeat) -> c_int;
type SeatName = unsafe extern "C" fn(*mut LibSeat) -> *const c_char;
type SwitchSession = unsafe extern "C" fn(*mut LibSeat, c_int) -> c_int;
type Dispatch = unsafe extern "C" fn(*mut LibSeat, c_int) -> c_int;
type OpenDevice =
    unsafe extern "C" fn(*mut LibSeat, *const c_char, *mut c_int) -> c_int;
type CloseDevice = unsafe extern "C" fn(*mut LibSeat, c_int) -> c_int;

struct Api {
    open_seat: OpenSeat,
    disable_seat: DisableSeat,
    close_seat: CloseSeat,
    seat_name: SeatName,
    switch_session: SwitchSession,
    dispatch: Dispatch,
    open_device: OpenDevice,
    close_device: CloseDevice,
}

//
// Types
//

// What the libseat callbacks need to reach. They fire from inside dispatch, on
// this thread, so a raw pointer handed to libseat as userdata is enough; there is
// no concurrent access to guard.
struct Shared {
    // The card, once we have it. Session control is taken before there is any card
    // at all, so the callbacks have to tolerate -1: they simply have no display to
    // hand over yet.
    drm_fd: i32,
    // Whether the DRM master handover is ours to perform. It is only ours for a card
    // we opened ourselves. For a card logind handed us, logind does it (setmaster on
    // activate, dropmaster on pause) and the kernel refuses ours anyway: SET_MASTER
    // from an unprivileged process needs the fd to have been opened by that same
    // process, and this one was opened by logind.
    own_master: bool,
    active: bool,
    // Set once the session has been activated for the first time.
    enabled: bool,
    // libseat's acknowledgement of a deactivation, called from the callback.
    disable_seat: DisableSeat,
}

pub struct Seat {
    api: Api,
    handle: *mut LibSeat,
    shared: *mut Shared,
    // libseat's id for the card, when we opened it through libseat. -1 when we did
    // not, in which case the fd is raylib's and only borrowed here.
    card: i32,
    // The VT logind gave our session.
    our_vt: u16,
    // Our VT's device node, only read for the list of VTs in use. -1 if we could
    // not open it.
    tty_fd: i32,
}

// struct drm_get_cap from <drm/drm.h>.
#[repr(C)]
struct DrmGetCap {
    capability: u64,
    value: u64,
}

// struct vt_stat from <linux/vt.h>.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct VtStat {
    v_active: u16,
    v_signal: u16,
    // Bitmask of VTs in use: bit n is set when VT n is allocated.
    v_state: u16,
}

//
// Callbacks
//

extern "C" fn on_enable(_seat: *mut LibSeat, data: *mut c_void) {
    let s = unsafe { &mut *(data as *mut Shared) };
    if s.own_master {
        take_master(s.drm_fd);
    }
    s.active = true;
    s.enabled = true;
}

extern "C" fn on_disable(seat: *mut LibSeat, data: *mut c_void) {
    let s = unsafe { &mut *(data as *mut Shared) };
    // Stop using the display before acknowledging: logind completes the switch as
    // soon as we do, and for a card it handed us it takes master away itself.
    if s.own_master {
        drop_master(s.drm_fd);
    }
    s.active = false;
    unsafe { (s.disable_seat)(seat) };
}

//
// Open
//

pub fn init() -> Option<Seat> {
    let api = load_api()?;

    let shared = Box::into_raw(Box::new(Shared {
        drm_fd: -1,
        own_master: false,
        active: false,
        enabled: false,
        disable_seat: api.disable_seat,
    }));
    let handle = unsafe { (api.open_seat)(&LISTENER, shared as *mut c_void) };
    if handle.is_null() {
        eprintln!(
            "om_wm: seat: no session to control (need a logind session or seatd), \
             falling back to grabbed input with no vt switching"
        );
        drop(unsafe { Box::from_raw(shared) });
        return None;
    }

    // The seat is not usable until libseat reports it enabled.
    let mut tries = 0;
    while !unsafe { (*shared).enabled } && tries < ENABLE_TRIES {
        if unsafe { (api.dispatch)(handle, ENABLE_TIMEOUT_MS) } < 0 {
            break;
        }
        tries += 1;
    }
    if !unsafe { (*shared).enabled } {
        eprintln!("om_wm: seat: session never became active, falling back");
        unsafe { (api.close_seat)(handle) };
        drop(unsafe { Box::from_raw(shared) });
        return None;
    }

    let our_vt = session_vt().unwrap_or(1);
    let tty_fd = open_vt_dev(our_vt);
    let name = unsafe {
        let p = (api.seat_name)(handle);
        if p.is_null() {
            "?".to_string()
        } else {
            std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
        }
    };
    println!(
        "om_wm: session control on {name}, rendering on vt{our_vt}, \
         switch with ctrl+alt+F1..F12 or alt+left/right, ctrl+alt+F{our_vt} comes back"
    );

    Some(Seat { api, handle, shared, card: -1, our_vt, tty_fd })
}

fn load_api() -> Option<Api> {
    let lib = unsafe {
        libc::dlopen(c"libseat.so.1".as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL)
    };
    if lib.is_null() {
        eprintln!("om_wm: seat: libseat.so.1 not available, no session control");
        return None;
    }
    Some(Api {
        open_seat: unsafe { std::mem::transmute(sym(lib, c"libseat_open_seat")?) },
        disable_seat: unsafe { std::mem::transmute(sym(lib, c"libseat_disable_seat")?) },
        close_seat: unsafe { std::mem::transmute(sym(lib, c"libseat_close_seat")?) },
        seat_name: unsafe { std::mem::transmute(sym(lib, c"libseat_seat_name")?) },
        switch_session: unsafe {
            std::mem::transmute(sym(lib, c"libseat_switch_session")?)
        },
        dispatch: unsafe { std::mem::transmute(sym(lib, c"libseat_dispatch")?) },
        open_device: unsafe { std::mem::transmute(sym(lib, c"libseat_open_device")?) },
        close_device: unsafe { std::mem::transmute(sym(lib, c"libseat_close_device")?) },
    })
}

fn sym(lib: *mut c_void, name: &std::ffi::CStr) -> Option<*mut c_void> {
    let p = unsafe { libc::dlsym(lib, name.as_ptr()) };
    if p.is_null() {
        eprintln!("om_wm: seat: libseat symbol missing: {}", name.to_string_lossy());
        return None;
    }
    Some(p)
}

//
// The card
//

// Open the KMS device through libseat, so logind is the one handing it out and the
// one revoking it on a switch. The fd is the caller's to give to raylib and stays
// open until shutdown. Returns None if there is no card or libseat refuses it, and
// the caller can then fall back to adopt_card.
pub fn open_card(s: &mut Seat) -> Option<i32> {
    let path = card_path()?;
    let c_path = CString::new(path.clone()).ok()?;
    let mut fd: c_int = -1;
    let id = unsafe { (s.api.open_device)(s.handle, c_path.as_ptr(), &mut fd) };
    if id < 0 || fd < 0 {
        let err = std::io::Error::last_os_error();
        eprintln!("om_wm: seat: libseat would not open {path} ({err})");
        return None;
    }
    s.card = id;
    unsafe { (*s.shared).drm_fd = fd };
    // No SET_MASTER here on purpose: logind takes master on this fd while our session
    // is active and drops it when it pauses the device, and our own attempt would be
    // refused. See own_master.
    println!("om_wm: seat: {path} through libseat, fd {fd}");
    Some(fd)
}

// The fallback: raylib opened the card itself, so find its fd and use that for the
// master handover. Everything still works, minus logind's pause-and-wait on a switch.
pub fn adopt_card(s: &mut Seat) -> bool {
    let fd = find_drm_fd();
    if fd < 0 {
        eprintln!("om_wm: seat: no /dev/dri/card* fd open, vt switches will not release the display");
        return false;
    }
    unsafe { (*s.shared).drm_fd = fd };
    unsafe { (*s.shared).own_master = true };
    if active(s) {
        take_master(fd);
    }
    true
}

// Whether logind is the one moving DRM master around, which it is for a card it
// opened for us. Callers use it to skip ownership checks that only make sense for a
// card we opened ourselves.
pub fn card_from_logind(s: &Seat) -> bool {
    s.card >= 0
}

// Which card to drive: the one with something plugged into it, since raylib's own
// guesswork (platform-gpu-card, then card1, then card0) can land on a headless one.
fn card_path() -> Option<String> {
    let mut cards: Vec<String> = Vec::new();
    for entry in fs::read_dir("/dev/dri").ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("card") {
            cards.push(name);
        }
    }
    cards.sort();
    for card in &cards {
        if card_is_connected(card) {
            return Some(format!("/dev/dri/{card}"));
        }
    }
    cards.first().map(|card| format!("/dev/dri/{card}"))
}

// Whether any of this card's connectors has a display on it. /sys/class/drm holds one
// directory per connector, named cardN-<connector>, each with a status file.
fn card_is_connected(card: &str) -> bool {
    let Ok(entries) = fs::read_dir("/sys/class/drm") else { return false };
    let prefix = format!("{card}-");
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with(&prefix) {
            continue;
        }
        if let Ok(status) = fs::read_to_string(entry.path().join("status")) {
            if status.trim() == "connected" {
                return true;
            }
        }
    }
    false
}

// Where raylib's own card fd is: it keeps it private, so find the /dev/dri/card*
// link in our own fd table. A card-shaped name is not enough to go on, because raylib
// leaks the fd of every card it probes and rejects (see its open chain: by-path, then
// card1, then card0), so each candidate has to answer a DRM ioctl before we use it for
// the master handover.
fn find_drm_fd() -> i32 {
    let Ok(entries) = fs::read_dir("/proc/self/fd") else { return -1 };
    for entry in entries.flatten() {
        let Ok(target) = fs::read_link(entry.path()) else { continue };
        let Some(path) = target.to_str() else { continue };
        if !path.starts_with("/dev/dri/card") {
            continue;
        }
        let Some(fd) = entry.file_name().to_str().and_then(|n| n.parse::<i32>().ok()) else {
            continue;
        };
        if is_drm_node(fd) {
            return fd;
        }
    }
    -1
}

// Whether this fd is a KMS node that answers DRM ioctls. Any capability would do; the
// dumb buffer one is the oldest and every driver knows it.
fn is_drm_node(fd: i32) -> bool {
    let mut cap = DrmGetCap { capability: DRM_CAP_DUMB_BUFFER, value: 0 };
    unsafe { libc::ioctl(fd, DRM_IOCTL_GET_CAP, &mut cap as *mut DrmGetCap) == 0 }
}

// The VT of our session. XDG_VTNR is what logind exports for it; the VT on screen
// at startup is the same thing, since a session we can control is the active one.
fn session_vt() -> Option<u16> {
    if let Some(n) = std::env::var("XDG_VTNR").ok().and_then(|v| v.parse::<u16>().ok()) {
        if n >= 1 && n <= MAX_VT {
            return Some(n);
        }
    }
    let text = fs::read_to_string("/sys/class/tty/tty0/active").ok()?;
    text.trim().strip_prefix("tty")?.parse().ok()
}

// Our VT's node, which logind chowns to us when we take control. Only used to ask
// the kernel which VTs exist, for alt+left/right.
fn open_vt_dev(vt: u16) -> i32 {
    let Ok(path) = CString::new(format!("/dev/tty{vt}")) else { return -1 };
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDWR | libc::O_NOCTTY) };
    if fd < 0 {
        return -1;
    }
    if read_state(fd).is_none() {
        unsafe { libc::close(fd) };
        return -1;
    }
    fd
}

//
// DRM master
//

fn take_master(drm_fd: i32) {
    if drm_fd < 0 {
        return;
    }
    if unsafe { libc::ioctl(drm_fd, DRM_IOCTL_SET_MASTER) } != 0 {
        let err = std::io::Error::last_os_error();
        eprintln!("om_wm: seat: set master failed: {err}");
    }
}

fn drop_master(drm_fd: i32) -> bool {
    drm_fd >= 0 && unsafe { libc::ioctl(drm_fd, DRM_IOCTL_DROP_MASTER) == 0 }
}

// Whether the display is actually ours to drive. raylib will open the card, build a
// GBM surface and report a healthy 1440x900 window without ever being DRM master:
// only the per-frame modeset fails, silently, so a second instance runs blind while
// holding the keyboard. Asking for master answers it properly, and asking is
// harmless when we already have it.
//
// Only meaningful for a card we opened ourselves. For a logind card the kernel refuses
// the question (see own_master) and there is nothing to ask anyway: logind gives the
// device to one session at a time, and a second taker is refused at open.
pub fn drm_is_ours() -> bool {
    let fd = find_drm_fd();
    if fd < 0 {
        return true; // No card found; whatever is wrong, it is not contention.
    }
    if unsafe { libc::ioctl(fd, DRM_IOCTL_SET_MASTER) } == 0 {
        return true;
    }
    let err = std::io::Error::last_os_error();
    eprintln!(
        "om_wm: the display belongs to another process ({err}). Only one thing can \
         drive DRM at a time: check 'pgrep -a om_wm'."
    );
    false
}

//
// Query
//

// False while another VT owns the display: no drawing, no input, no frame
// callbacks until it comes back.
pub fn active(s: &Seat) -> bool {
    unsafe { (*s.shared).active }
}

pub fn our_vt(s: &Seat) -> u16 {
    s.our_vt
}

// Pump libseat. Activation and deactivation land in the callbacks from here.
pub fn poll(s: &Seat) {
    if unsafe { (s.api.dispatch)(s.handle, 0) } < 0 {
        let err = std::io::Error::last_os_error();
        eprintln!("om_wm: seat: dispatch failed: {err}");
    }
}

fn read_state(tty_fd: i32) -> Option<VtStat> {
    let mut st = VtStat::default();
    let ok = unsafe { libc::ioctl(tty_fd, VT_GETSTATE, &mut st as *mut VtStat) == 0 };
    if ok && st.v_active != 0 {
        Some(st)
    } else {
        None
    }
}

//
// Chord decoding
//

// Decode a VT switch request from this frame's key press edges. The console does not act on
// these itself while our VT is in K_OFF, so they are ours to implement: ctrl+alt+F1..F12 picks a
// VT directly.
//
// Alt+left and alt+right used to step to the previous and next VT in use, the way the console
// does. They are not ours to take. Alt with an arrow is back and forward in a browser, and word
// motion in an editor, and a compositor that swallows it to change VT is taking a chord people
// use constantly to do something they meant once a week. Ctrl+alt+Fn is the deliberate one and
// it is enough.
pub fn chord(_s: &Seat, events: &[(u16, bool)], ctrl: bool, alt: bool) -> Option<u16> {
    if !alt || !ctrl {
        return None;
    }
    for &(code, pressed) in events {
        if !pressed {
            continue;
        }
        if let Some(n) = fkey_vt(code) {
            return Some(n);
        }
    }
    None
}

fn fkey_vt(code: u16) -> Option<u16> {
    match code {
        input::KEY_F1..=input::KEY_F10 => Some(code - input::KEY_F1 + 1),
        input::KEY_F11 => Some(11),
        input::KEY_F12 => Some(12),
        _ => None,
    }
}

//
// Switch
//

// Hand the display to another VT. We drop master before asking, because logind
// does not wait for us: it only pauses and waits for devices opened through
// libseat, which is stage 2. Callers stop drawing until active() is true again.
pub fn switch_to(s: &Seat, target: u16) -> bool {
    if target == 0 || target == s.our_vt || !active(s) {
        return false;
    }
    // For a logind card this is logind's job, and it does it before the switch
    // completes; for our own card, stop driving the display first.
    let own = unsafe { (*s.shared).own_master };
    let dropped = own && drop_master(unsafe { (*s.shared).drm_fd });
    if unsafe { (s.api.switch_session)(s.handle, target as c_int) } < 0 {
        let err = std::io::Error::last_os_error();
        eprintln!("om_wm: seat: switch to vt{target} failed: {err}");
        if dropped {
            take_master(unsafe { (*s.shared).drm_fd });
        }
        return false;
    }
    unsafe { (*s.shared).active = false };
    true
}

// Release the session. logind puts our VT back to KD_TEXT and K_XLATE when the
// connection goes, so this is tidiness rather than a requirement.
pub fn shutdown(s: &mut Seat) {
    if s.tty_fd >= 0 {
        unsafe { libc::close(s.tty_fd) };
        s.tty_fd = -1;
    }
    // Ours to close only if libseat gave it to us. Call this after the window is
    // closed: raylib is still drawing through this fd until then.
    if s.card >= 0 {
        let fd = unsafe { (*s.shared).drm_fd };
        unsafe { (s.api.close_device)(s.handle, s.card) };
        if fd >= 0 {
            unsafe { libc::close(fd) };
        }
        s.card = -1;
        unsafe { (*s.shared).drm_fd = -1 };
    }
    unsafe { (s.api.close_seat)(s.handle) };
    drop(unsafe { Box::from_raw(s.shared) });
    s.shared = std::ptr::null_mut();
}
