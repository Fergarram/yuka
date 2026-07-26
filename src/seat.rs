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
// Stage 2, when raylib can be handed an externally opened DRM fd: take the card
// and the input devices through libseat_open_device, so logind pauses them and
// waits for our ack before completing a switch. Today it does not wait, so an
// outside switch can beat us to the display by a few milliseconds.
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

struct Api {
    open_seat: OpenSeat,
    disable_seat: DisableSeat,
    close_seat: CloseSeat,
    seat_name: SeatName,
    switch_session: SwitchSession,
    dispatch: Dispatch,
}

//
// Types
//

// What the libseat callbacks need to reach. They fire from inside dispatch, on
// this thread, so a raw pointer handed to libseat as userdata is enough; there is
// no concurrent access to guard.
struct Shared {
    drm_fd: i32,
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
    // The VT logind gave our session.
    our_vt: u16,
    // Our VT's device node, only read for the list of VTs in use. -1 if we could
    // not open it.
    tty_fd: i32,
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
    take_master(s.drm_fd);
    s.active = true;
    s.enabled = true;
}

extern "C" fn on_disable(seat: *mut LibSeat, data: *mut c_void) {
    let s = unsafe { &mut *(data as *mut Shared) };
    // Stop using the display before acknowledging: logind may complete the
    // switch as soon as we do.
    drop_master(s.drm_fd);
    s.active = false;
    unsafe { (s.disable_seat)(seat) };
}

//
// Open
//

pub fn init() -> Option<Seat> {
    let drm_fd = find_drm_fd();
    if drm_fd < 0 {
        eprintln!("om_wm: seat: no /dev/dri/card* fd open, session control disabled");
        return None;
    }
    let api = load_api()?;

    let shared = Box::into_raw(Box::new(Shared {
        drm_fd,
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

    Some(Seat { api, handle, shared, our_vt, tty_fd })
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

// raylib opens the KMS device itself and keeps the fd private, so find it by
// looking for the /dev/dri/card* link in our own fd table.
fn find_drm_fd() -> i32 {
    let Ok(entries) = fs::read_dir("/proc/self/fd") else { return -1 };
    for entry in entries.flatten() {
        let Ok(target) = fs::read_link(entry.path()) else { continue };
        let Some(name) = target.file_name().and_then(|n| n.to_str()) else { continue };
        if !name.starts_with("card") {
            continue;
        }
        if let Some(fd) = entry.file_name().to_str().and_then(|n| n.parse::<i32>().ok()) {
            return fd;
        }
    }
    -1
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
    if unsafe { libc::ioctl(drm_fd, DRM_IOCTL_SET_MASTER) } != 0 {
        let err = std::io::Error::last_os_error();
        eprintln!("om_wm: seat: set master failed: {err}");
    }
}

fn drop_master(drm_fd: i32) -> bool {
    unsafe { libc::ioctl(drm_fd, DRM_IOCTL_DROP_MASTER) == 0 }
}

// Whether the display is actually ours to drive. raylib will open the card, build a
// GBM surface and report a healthy 1440x900 window without ever being DRM master:
// only the per-frame modeset fails, silently, so a second instance runs blind while
// holding the keyboard. Asking for master answers it properly, and asking is
// harmless when we already have it.
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

// Decode a VT switch request from this frame's key press edges. The console does
// not act on these itself while our VT is in K_OFF, so they are ours to
// implement: ctrl+alt+F1..F12 picks a VT directly, alt+left / alt+right step to
// the previous/next VT in use, the way the console would.
pub fn chord(s: &Seat, events: &[(u16, bool)], ctrl: bool, alt: bool) -> Option<u16> {
    if !alt {
        return None;
    }
    for &(code, pressed) in events {
        if !pressed {
            continue;
        }
        if ctrl {
            if let Some(n) = fkey_vt(code) {
                return Some(n);
            }
        }
        match code {
            input::KEY_LEFT => return step(s, -1),
            input::KEY_RIGHT => return step(s, 1),
            _ => {}
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

// The next VT in use, walking in dir from ours and wrapping around. With no VT
// node to ask, step by one and let the kernel allocate what is not there.
fn step(s: &Seat, dir: i32) -> Option<u16> {
    let mask = if s.tty_fd >= 0 {
        read_state(s.tty_fd).map(|st| st.v_state).unwrap_or(0)
    } else {
        0
    };
    let mut n = s.our_vt as i32;
    for _ in 0..MAX_VT {
        n += dir;
        if n < 1 {
            n = MAX_VT as i32;
        }
        if n > MAX_VT as i32 {
            n = 1;
        }
        if mask == 0 || mask & (1 << n) != 0 {
            return Some(n as u16);
        }
    }
    None
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
    let dropped = drop_master(unsafe { (*s.shared).drm_fd });
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
    unsafe { (s.api.close_seat)(s.handle) };
    drop(unsafe { Box::from_raw(s.shared) });
    s.shared = std::ptr::null_mut();
}
