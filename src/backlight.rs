//
// Backlight (Data Oriented zone)
//
// The panel's backlight, which the kernel shows as two numbers under /sys/class/backlight:
// brightness, and the max_brightness it goes up to. Writing the first changes the light, but
// the file is root's. logind lets the session in front of the screen set it anyway
// (org.freedesktop.login1.Session.SetBrightness), and yuka is that session, so this asks
// logind over D-Bus and needs no root, no group and no udev rule. Where logind will not, a
// brightness file we may write is the fallback.
//
// The scripts choose the level (yk.backlight, lua/backlight.lua); this module only finds the
// device, sends the value, and reads it back now and then so a change made elsewhere shows.
//
// libsystemd is loaded with dlopen, as libseat is, so it stays a runtime option. All the
// dlopen/FFI and raw pointer work is contained here.
//

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::fs;
use std::time::{Duration, Instant};

//
// Constants
//

// Where the light goes when it is off and something has to be read: the scripts broke, or
// yuka is quitting with no level from before to go back to. A fraction of max_brightness.
const RESCUE_FRACTION: f32 = 0.2;

// How often the level is read back, to notice brightness keys the firmware handles itself or
// another program setting it.
const POLL_EVERY: Duration = Duration::from_millis(500);

// How long after we set it before reading back, since logind applies it in its own time and
// an early read would see the old level.
const QUIET_AFTER_SET: Duration = Duration::from_millis(1000);

// Device types in the order to prefer them: firmware (ACPI) and platform interfaces know
// about the panel, raw is the GPU's register and the last resort.
const KINDS: [&str; 3] = ["firmware", "platform", "raw"];

//
// sd-bus FFI
//

#[repr(C)]
struct SdBusError {
    name: *const c_char,
    message: *const c_char,
    need_free: c_int,
}

type Handler = extern "C" fn(*mut c_void, *mut c_void, *mut SdBusError) -> c_int;

type OpenSystem = unsafe extern "C" fn(*mut *mut c_void) -> c_int;
type CallMethod = unsafe extern "C" fn(
    *mut c_void,
    *const c_char,
    *const c_char,
    *const c_char,
    *const c_char,
    *mut SdBusError,
    *mut *mut c_void,
    *const c_char,
    ...
) -> c_int;
type CallMethodAsync = unsafe extern "C" fn(
    *mut c_void,
    *mut *mut c_void,
    *const c_char,
    *const c_char,
    *const c_char,
    *const c_char,
    Option<Handler>,
    *mut c_void,
    *const c_char,
    ...
) -> c_int;
type Process = unsafe extern "C" fn(*mut c_void, *mut *mut c_void) -> c_int;
type Flush = unsafe extern "C" fn(*mut c_void) -> c_int;
type FlushCloseUnref = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
type MessageUnref = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
type ErrorFree = unsafe extern "C" fn(*mut SdBusError);
type IsMethodError = unsafe extern "C" fn(*mut c_void, *const c_char) -> c_int;
type GetError = unsafe extern "C" fn(*mut c_void) -> *const SdBusError;

struct Bus {
    bus: *mut c_void,
    call_method: CallMethod,
    call_method_async: CallMethodAsync,
    process: Process,
    flush: Flush,
    flush_close_unref: FlushCloseUnref,
    message_unref: MessageUnref,
    error_free: ErrorFree,
    // Boxed so its address stays put for the reply handler, which gets it as userdata.
    reply: Box<ReplyApi>,
}

// What the reply handler reads errors with.
struct ReplyApi {
    is_method_error: IsMethodError,
    get_error: GetError,
}

//
// Types
//

pub struct Backlight {
    // The device's name, which is what logind is asked about, and its brightness file.
    name: CString,
    path: String,
    // The level now, as we last set it or read it, and the most it goes to.
    pub raw: u32,
    pub max: u32,
    // The level found at start, to go back to on quit if the light is off then.
    start: u32,
    // How the level is set: logind when it will, else the file.
    bus: Option<Bus>,
    next_poll: Instant,
}

//
// Functions
//

// None nested, without a backlight, or when neither logind nor the file will take a value.
pub fn init() -> Option<Backlight> {
    let (name, dir) = find_device()?;
    let path = format!("{dir}/brightness");
    let max = read_u32(&format!("{dir}/max_brightness"))?;
    let raw = read_u32(&path)?;
    if max == 0 {
        return None;
    }
    let lib = unsafe { libc::dlopen(c"libsystemd.so.0".as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
    let bus = if lib.is_null() { None } else { load_bus(lib) };
    let name = CString::new(name).ok()?;
    let mut b = Backlight { name, path, raw, max, start: raw, bus, next_poll: Instant::now() };

    // Asking for the level it already has tells us whether logind will take one at all.
    if let Some(bus) = b.bus.as_ref() {
        if let Err(e) = set_sync(bus, &b.name, raw) {
            eprintln!("yuka: backlight: logind will not set it ({e})");
            let bus = b.bus.take().unwrap();
            unsafe { (bus.flush_close_unref)(bus.bus) };
        }
    }
    let how = if b.bus.is_some() {
        "through logind"
    } else if fs::write(&b.path, raw.to_string()).is_ok() {
        "by writing the file"
    } else {
        eprintln!("yuka: backlight: no way to set {}, leaving it alone", b.path);
        return None;
    };
    println!("yuka: backlight {} at {raw} of {max}, set {how}", b.name.to_string_lossy());
    Some(b)
}

// Set the level as a fraction of the most, 0 for off. Only sent when it lands on a different
// step from the one the panel is on.
pub fn set(b: &mut Backlight, fraction: f32) {
    let f = if fraction.is_finite() { fraction.clamp(0.0, 1.0) } else { 1.0 };
    let raw = ((f * b.max as f32).round() as u32).min(b.max);
    if raw == b.raw {
        return;
    }
    b.raw = raw;
    b.next_poll = Instant::now() + QUIET_AFTER_SET;
    if let Some(bus) = b.bus.as_ref() {
        drain(bus);
        // Not waited for, so a sweep does not stall a frame on every step. Errors come back
        // to the handler.
        let reply = &*bus.reply as *const ReplyApi as *mut c_void;
        let r = unsafe {
            (bus.call_method_async)(
                bus.bus,
                std::ptr::null_mut(),
                c"org.freedesktop.login1".as_ptr(),
                c"/org/freedesktop/login1/session/auto".as_ptr(),
                c"org.freedesktop.login1.Session".as_ptr(),
                c"SetBrightness".as_ptr(),
                Some(on_reply as Handler),
                reply,
                c"ssu".as_ptr(),
                c"backlight".as_ptr(),
                b.name.as_ptr(),
                raw,
            )
        };
        if r < 0 {
            eprintln!("yuka: backlight: SetBrightness not sent ({})", std::io::Error::from_raw_os_error(-r));
        }
        unsafe { (bus.flush)(bus.bus) };
    } else if let Err(e) = fs::write(&b.path, raw.to_string()) {
        eprintln!("yuka: backlight: {} not written ({e})", b.path);
    }
}

// Once a frame: answer logind's replies, and now and then read the level back.
pub fn poll(b: &mut Backlight) {
    if let Some(bus) = b.bus.as_ref() {
        drain(bus);
    }
    let now = Instant::now();
    if now < b.next_poll {
        return;
    }
    b.next_poll = now + POLL_EVERY;
    if let Some(raw) = read_u32(&b.path) {
        b.raw = raw;
    }
}

// The light back on, if it is off, for when the scripts that turned it off are broken and the
// error banner has to be read.
pub fn rescue(b: &mut Backlight) {
    if b.raw == 0 {
        println!("yuka: backlight: off while the scripts are broken, turning it back on");
        set(b, RESCUE_FRACTION);
    }
}

// On quit: off is no way to leave a console, so the level from before comes back.
pub fn destroy(b: &mut Backlight) {
    // Waited for, since logind may not know whose request it was once we are gone.
    if b.raw == 0 {
        let raw = if b.start > 0 { b.start } else { (RESCUE_FRACTION * b.max as f32) as u32 };
        let done = match b.bus.as_ref() {
            Some(bus) => set_sync(bus, &b.name, raw),
            None => fs::write(&b.path, raw.to_string()).map_err(|e| e.to_string()),
        };
        if let Err(e) = done {
            eprintln!("yuka: backlight: could not turn it back on ({e})");
        }
    }
    if let Some(bus) = b.bus.take() {
        unsafe { (bus.flush_close_unref)(bus.bus) };
    }
}

fn drain(bus: &Bus) {
    while unsafe { (bus.process)(bus.bus, std::ptr::null_mut()) } > 0 {}
}

fn set_sync(bus: &Bus, name: &CStr, raw: u32) -> Result<(), String> {
    let mut err = SdBusError { name: std::ptr::null(), message: std::ptr::null(), need_free: 0 };
    let mut reply: *mut c_void = std::ptr::null_mut();
    let r = unsafe {
        (bus.call_method)(
            bus.bus,
            c"org.freedesktop.login1".as_ptr(),
            c"/org/freedesktop/login1/session/auto".as_ptr(),
            c"org.freedesktop.login1.Session".as_ptr(),
            c"SetBrightness".as_ptr(),
            &mut err,
            &mut reply,
            c"ssu".as_ptr(),
            c"backlight".as_ptr(),
            name.as_ptr(),
            raw,
        )
    };
    let out = if r < 0 {
        Err(if err.message.is_null() {
            std::io::Error::from_raw_os_error(-r).to_string()
        } else {
            unsafe { CStr::from_ptr(err.message) }.to_string_lossy().into_owned()
        })
    } else {
        Ok(())
    };
    unsafe {
        (bus.error_free)(&mut err);
        if !reply.is_null() {
            (bus.message_unref)(reply);
        }
    }
    out
}

extern "C" fn on_reply(m: *mut c_void, userdata: *mut c_void, _err: *mut SdBusError) -> c_int {
    let api = unsafe { &*(userdata as *const ReplyApi) };
    if unsafe { (api.is_method_error)(m, std::ptr::null()) } > 0 {
        let e = unsafe { (api.get_error)(m) };
        let msg = if e.is_null() || unsafe { (*e).message }.is_null() {
            "no reason given".to_string()
        } else {
            unsafe { CStr::from_ptr((*e).message) }.to_string_lossy().into_owned()
        };
        eprintln!("yuka: backlight: logind refused SetBrightness ({msg})");
    }
    0
}

fn load_bus(lib: *mut c_void) -> Option<Bus> {
    let reply = Box::new(ReplyApi {
        is_method_error: unsafe { std::mem::transmute(sym(lib, c"sd_bus_message_is_method_error")?) },
        get_error: unsafe { std::mem::transmute(sym(lib, c"sd_bus_message_get_error")?) },
    });
    let open: OpenSystem = unsafe { std::mem::transmute(sym(lib, c"sd_bus_open_system")?) };
    let mut handle: *mut c_void = std::ptr::null_mut();
    if unsafe { open(&mut handle) } < 0 || handle.is_null() {
        eprintln!("yuka: backlight: no system bus");
        return None;
    }
    Some(Bus {
        bus: handle,
        call_method: unsafe { std::mem::transmute(sym(lib, c"sd_bus_call_method")?) },
        call_method_async: unsafe { std::mem::transmute(sym(lib, c"sd_bus_call_method_async")?) },
        process: unsafe { std::mem::transmute(sym(lib, c"sd_bus_process")?) },
        flush: unsafe { std::mem::transmute(sym(lib, c"sd_bus_flush")?) },
        flush_close_unref: unsafe { std::mem::transmute(sym(lib, c"sd_bus_flush_close_unref")?) },
        message_unref: unsafe { std::mem::transmute(sym(lib, c"sd_bus_message_unref")?) },
        error_free: unsafe { std::mem::transmute(sym(lib, c"sd_bus_error_free")?) },
        reply,
    })
}

fn sym(lib: *mut c_void, name: &CStr) -> Option<*mut c_void> {
    let p = unsafe { libc::dlsym(lib, name.as_ptr()) };
    if p.is_null() {
        eprintln!("yuka: backlight: libsystemd symbol missing: {}", name.to_string_lossy());
        return None;
    }
    Some(p)
}

// The backlight to drive, by KINDS, as its name and its directory.
fn find_device() -> Option<(String, String)> {
    let mut found: Vec<(usize, String)> = Vec::new();
    for entry in fs::read_dir("/sys/class/backlight").ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let kind = fs::read_to_string(format!("/sys/class/backlight/{name}/type")).unwrap_or_default();
        let rank = KINDS.iter().position(|k| *k == kind.trim()).unwrap_or(KINDS.len());
        found.push((rank, name));
    }
    found.sort();
    let (_, name) = found.into_iter().next()?;
    let dir = format!("/sys/class/backlight/{name}");
    Some((name, dir))
}

fn read_u32(path: &str) -> Option<u32> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}
