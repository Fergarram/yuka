//
// Lua boundary
//
// The scripts are where yuka's behaviour lives: what a key does, where a scroll goes,
// what the settings are. The engine runs them, hands them a snapshot of the frame, and
// takes back a list of commands. Nothing in here decides anything.
//
// Like the Smithay zone, this module is shaped by an external API: mlua wants its own
// traits and reference counted handles, and they stay in here. What crosses out is
// plain data, a Settings, a Vec<Cmd> and a Route, so the rest of the engine never knows
// a script exists.
//
// Reads are a snapshot, writes are commands. A script never holds a reference into
// engine state: it sees numbers and window handles (render::Windows ids), and whatever
// it wants done goes into a buffer the engine drains after the call returns. That keeps
// the engine the only writer, makes the order of effects the order of the buffer, and
// means a script that misbehaves can only ever ask for things.
//
// yuka does not run without scripts: if they will not load at startup, init says so
// and the engine exits. Reloading builds a fresh VM, and one that fails to load leaves
// the running one in place with its error kept for the screen, so a bad save costs
// nothing but a message.
//

use std::time::SystemTime;

use mlua::{Function, Lua, Table, Value};

use crate::settings::{self, Settings};

//
// Constants
//

// Where the scripts live, unless YUKA_LUA says otherwise.
pub const DEFAULT_DIR: &str = "lua";
// The entry script, inside that directory.
const ENTRY: &str = "init.lua";
// How many key events a frame is expected to carry. The snapshot arrays are sized for this
// once and grow if a frame ever has more.
const KEYS_HINT: usize = 16;
// Same count of keycodes input.rs tracks.
const KEY_CODES: usize = crate::input::KEY_CODES;

//
// Types
//

// Debug overlays a script can switch.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Debug {
    Labels,
    Fps,
    Pad,
}

// What a script can ask the engine to do. Drained in order after each call.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Cmd {
    // Re-read the scripts, as a save would.
    Reload,
    ToggleDebug(Debug),
    // Ask a window to close. The handle is a render::Windows id.
    Close(u64),
    Screenshot,
    // Back to the default zoom, around the middle of the screen or a screen point.
    ZoomReset,
    ZoomResetAt(f32, f32),
    // Scale the view by a factor, holding a screen point still.
    ZoomAt(f32, f32, f32),
    // Move the view, in screen pixels. Positive x shows more of what is to the right.
    Pan(f32, f32),
    // Let go of every window: keyboard, pointer and implicit grab.
    ReleaseAll,
}

// Where this frame's scrolling and pinching go. true means the client under the pointer,
// false means nobody: what the canvas does with it is a Cmd of its own.
//
// Only the destination. The protocol rules that come with sending to a client (axis
// sequences that must be stopped, pinch sequences that must be ended) stay in the engine,
// so a script cannot leave a client believing a gesture is still running.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct Route {
    pub wheel: bool,
    pub fingers: bool,
    pub pinch: bool,
}

// The frame as a script sees it. Filled by the main loop, copied into the VM once per
// frame.
pub struct FrameIn<'a> {
    // Key edges this frame as (evdev code, pressed), and every key's level.
    pub keys: &'a [(u16, bool)],
    pub held: &'a [bool],
    pub super_down: bool,
    pub ctrl_down: bool,
    pub alt_down: bool,
    pub shift_down: bool,
    // Screen position of the pointer, when there is one.
    pub pointer: Option<(f32, f32)>,
    // Pointer motion this frame, and the mouse's wheels and middle button.
    pub dx: f32,
    pub dy: f32,
    pub wheel: f32,
    pub hwheel: f32,
    pub middle: bool,
    // Finger scroll this frame from whichever code drives the pad, positive up and right.
    pub scroll_x: f32,
    pub scroll_y: f32,
    // The fingers making a scroll have lifted, as libinput reports it.
    pub scroll_lifted: bool,
    // Our own pad code is driving, and how many contacts it sees.
    pub pad_driving: bool,
    pub fingers: u32,
    // Pinch factor this frame, 1.0 for none.
    pub pinch: f32,
    // Window handles, or None.
    pub focused: Option<u64>,
    pub hovered: Option<u64>,
    // The view.
    pub zoom: f32,
    pub cx: f32,
    pub cy: f32,
    pub sw: f32,
    pub sh: f32,
    pub dt: f32,
    pub frame: u32,
}

// What the functions a script calls write into. Lives in the VM's app data, so the
// callbacks need no shared ownership to reach it.
struct Io {
    out: Vec<Cmd>,
    route: Route,
    held: Vec<bool>,
}

// One loaded script set: the VM and the handles the frame call reuses, so a frame
// allocates no tables.
struct Vm {
    lua: Lua,
    frame_fn: Function,
    snap: Table,
    key_code: Table,
    key_press: Table,
}

pub struct Script {
    dir: String,
    vm: Vm,
    // Newest modification time under dir as of the last load, to notice a save.
    stamp: Option<SystemTime>,
    // Why the last load failed, or why the last frame call did. Shown on screen until a
    // load or a frame succeeds.
    pub error: Option<String>,
}

//
// Load
//

// The first load. Err with the reason when the scripts will not load, which the caller
// treats as fatal.
pub fn init(dir: &str) -> Result<(Script, Settings), String> {
    let stamp = newest_mtime(dir);
    let (vm, pairs) = load_vm(dir).map_err(|e| format!("{dir}/{ENTRY}: {e}"))?;
    let set = adopt_pairs(dir, &pairs);
    Ok((Script { dir: dir.to_string(), vm, stamp, error: None }, set))
}

// Build a fresh VM from the scripts on disk. Some(settings) when it loaded and the new VM
// is in force; None when it did not, and the scripts that were running still are.
pub fn reload(s: &mut Script) -> Option<Settings> {
    s.stamp = newest_mtime(&s.dir);
    match load_vm(&s.dir) {
        Ok((vm, pairs)) => {
            s.vm = vm;
            s.error = None;
            Some(adopt_pairs(&s.dir, &pairs))
        }
        Err(e) => {
            let msg = format!("{}/{ENTRY}: {e}", s.dir);
            eprintln!("yuka: lua: {msg}");
            eprintln!("yuka: lua: keeping the scripts that were running");
            s.error = Some(msg);
            None
        }
    }
}

fn adopt_pairs(dir: &str, pairs: &[(String, String)]) -> Settings {
    let origin = format!("{dir}/config.lua");
    let (set, applied) = settings::from_pairs(pairs, &origin);
    println!("yuka: lua: loaded {dir}/{ENTRY}, {applied} settings");
    set
}

// Reload if anything under the script directory has been saved since the last load.
pub fn reload_if_changed(s: &mut Script) -> Option<Settings> {
    if newest_mtime(&s.dir) == s.stamp {
        return None;
    }
    reload(s)
}

fn load_vm(dir: &str) -> mlua::Result<(Vm, Vec<(String, String)>)> {
    let lua = Lua::new();
    lua.set_app_data(Io {
        out: Vec::with_capacity(32),
        route: Route::default(),
        held: vec![false; KEY_CODES],
    });

    // require() finds the other scripts next to the entry.
    let package: Table = lua.globals().get("package")?;
    package.set("path", format!("{dir}/?.lua;{dir}/?/init.lua"))?;

    let yk = lua.create_table()?;
    register(&lua, &yk)?;
    lua.globals().set("yk", yk.clone())?;

    let path = format!("{dir}/{ENTRY}");
    let source = std::fs::read_to_string(&path)
        .map_err(|e| mlua::Error::RuntimeError(format!("cannot read: {e}")))?;
    lua.load(source).set_name(format!("@{path}")).exec()?;

    let frame_fn: Function = match yk.get::<Value>("frame")? {
        Value::Function(f) => f,
        _ => return Err(mlua::Error::RuntimeError("defines no yk.frame function".into())),
    };
    let pairs = match yk.get::<Value>("config")? {
        Value::Table(t) => config_pairs(&t)?,
        Value::Nil => Vec::new(),
        _ => return Err(mlua::Error::RuntimeError("yk.config is not a table".into())),
    };

    let snap = lua.create_table_with_capacity(0, 32)?;
    let key_code = lua.create_table_with_capacity(KEYS_HINT, 0)?;
    let key_press = lua.create_table_with_capacity(KEYS_HINT, 0)?;
    snap.raw_set("key_code", key_code.clone())?;
    snap.raw_set("key_press", key_press.clone())?;
    Ok((Vm { lua, frame_fn, snap, key_code, key_press }, pairs))
}

// The config table flattened to the key and value strings settings.rs parses. Lists become
// comma separated, which is how settings.rs reads its one list (spawn_order).
fn config_pairs(t: &Table) -> mlua::Result<Vec<(String, String)>> {
    let mut pairs = Vec::with_capacity(96);
    for entry in t.pairs::<String, Value>() {
        let (key, value) = entry?;
        let text = match value {
            Value::Boolean(b) => b.to_string(),
            Value::Integer(n) => n.to_string(),
            Value::Number(n) => n.to_string(),
            Value::String(s) => s.to_str()?.to_string(),
            Value::Table(list) => {
                let mut parts: Vec<String> = Vec::new();
                for item in list.sequence_values::<String>() {
                    parts.push(item?);
                }
                parts.join(",")
            }
            other => {
                eprintln!("yuka: lua: config.{key} is a {}, skipped", other.type_name());
                continue;
            }
        };
        pairs.push((key, text));
    }
    Ok(pairs)
}

fn newest_mtime(dir: &str) -> Option<SystemTime> {
    let mut newest: Option<SystemTime> = None;
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(t) = meta.modified() else { continue };
        if newest.map_or(true, |n| t > n) {
            newest = Some(t);
        }
    }
    newest
}

//
// Functions scripts call
//

fn push(lua: &Lua, cmd: Cmd) {
    if let Some(mut io) = lua.app_data_mut::<Io>() {
        io.out.push(cmd);
    }
}

fn debug_named(name: &str) -> mlua::Result<Debug> {
    match name {
        "labels" => Ok(Debug::Labels),
        "fps" => Ok(Debug::Fps),
        "pad" => Ok(Debug::Pad),
        _ => Err(mlua::Error::RuntimeError(format!(
            "no debug overlay called '{name}' (labels, fps, pad)"
        ))),
    }
}

fn register(lua: &Lua, yk: &Table) -> mlua::Result<()> {
    yk.set("reload", lua.create_function(|lua, ()| {
        push(lua, Cmd::Reload);
        Ok(())
    })?)?;
    yk.set("toggle_debug", lua.create_function(|lua, name: String| {
        push(lua, Cmd::ToggleDebug(debug_named(&name)?));
        Ok(())
    })?)?;
    yk.set("close", lua.create_function(|lua, id: Option<u64>| {
        if let Some(id) = id {
            push(lua, Cmd::Close(id));
        }
        Ok(())
    })?)?;
    yk.set("screenshot", lua.create_function(|lua, ()| {
        push(lua, Cmd::Screenshot);
        Ok(())
    })?)?;
    yk.set("zoom_reset", lua.create_function(|lua, (x, y): (Option<f32>, Option<f32>)| {
        push(lua, match (x, y) {
            (Some(x), Some(y)) => Cmd::ZoomResetAt(x, y),
            _ => Cmd::ZoomReset,
        });
        Ok(())
    })?)?;
    yk.set("zoom_at", lua.create_function(|lua, (factor, x, y): (f32, f32, f32)| {
        push(lua, Cmd::ZoomAt(factor, x, y));
        Ok(())
    })?)?;
    yk.set("pan", lua.create_function(|lua, (dx, dy): (f32, f32)| {
        push(lua, Cmd::Pan(dx, dy));
        Ok(())
    })?)?;
    yk.set("release_all", lua.create_function(|lua, ()| {
        push(lua, Cmd::ReleaseAll);
        Ok(())
    })?)?;
    yk.set("route", lua.create_function(|lua, (wheel, fingers, pinch): (bool, bool, bool)| {
        if let Some(mut io) = lua.app_data_mut::<Io>() {
            io.route = Route { wheel, fingers, pinch };
        }
        Ok(())
    })?)?;
    yk.set("key_down", lua.create_function(|lua, code: usize| {
        let io = lua.app_data_ref::<Io>();
        Ok(io.map_or(false, |io| code < io.held.len() && io.held[code]))
    })?)?;
    Ok(())
}

//
// Frame
//

// Hand the frame to the script and collect what it asks for. Commands are appended to
// out; the route comes back, and sends nothing to clients unless the script says so.
pub fn frame(s: &mut Script, f: &FrameIn, out: &mut Vec<Cmd>) -> Route {
    let vm = &s.vm;

    if let Some(mut io) = vm.lua.app_data_mut::<Io>() {
        io.out.clear();
        io.route = Route::default();
        let n = f.held.len().min(io.held.len());
        io.held[..n].copy_from_slice(&f.held[..n]);
    }

    let result = fill_snapshot(vm, f).and_then(|()| vm.frame_fn.call::<()>(vm.snap.clone()));

    let mut route = Route::default();
    if let Some(mut io) = vm.lua.app_data_mut::<Io>() {
        out.extend_from_slice(&io.out);
        io.out.clear();
        route = io.route;
    }
    match result {
        Ok(()) => {
            // A frame that ran clears a runtime error, though not a failed reload: that one
            // stands until the scripts load.
            if s.error.as_deref().is_some_and(|e| e.starts_with("frame: ")) {
                s.error = None;
            }
        }
        Err(e) => {
            let msg = format!("frame: {e}");
            // Once per distinct error, since a broken frame function breaks every frame.
            if s.error.as_deref() != Some(msg.as_str()) {
                eprintln!("yuka: lua: {msg}");
                s.error = Some(msg);
            }
        }
    }
    route
}

fn fill_snapshot(vm: &Vm, f: &FrameIn) -> mlua::Result<()> {
    let t = &vm.snap;
    for (i, &(code, pressed)) in f.keys.iter().enumerate() {
        vm.key_code.raw_set(i + 1, code)?;
        vm.key_press.raw_set(i + 1, pressed)?;
    }
    t.raw_set("key_count", f.keys.len())?;
    t.raw_set("super", f.super_down)?;
    t.raw_set("ctrl", f.ctrl_down)?;
    t.raw_set("alt", f.alt_down)?;
    t.raw_set("shift", f.shift_down)?;
    match f.pointer {
        Some((x, y)) => {
            t.raw_set("x", x)?;
            t.raw_set("y", y)?;
        }
        None => {
            t.raw_set("x", Value::Nil)?;
            t.raw_set("y", Value::Nil)?;
        }
    }
    t.raw_set("dx", f.dx)?;
    t.raw_set("dy", f.dy)?;
    t.raw_set("wheel", f.wheel)?;
    t.raw_set("hwheel", f.hwheel)?;
    t.raw_set("middle", f.middle)?;
    t.raw_set("scroll_x", f.scroll_x)?;
    t.raw_set("scroll_y", f.scroll_y)?;
    t.raw_set("scroll_lifted", f.scroll_lifted)?;
    t.raw_set("pad_driving", f.pad_driving)?;
    t.raw_set("fingers", f.fingers)?;
    t.raw_set("pinch", f.pinch)?;
    t.raw_set("focused", f.focused)?;
    t.raw_set("hovered", f.hovered)?;
    t.raw_set("zoom", f.zoom)?;
    t.raw_set("cx", f.cx)?;
    t.raw_set("cy", f.cy)?;
    t.raw_set("sw", f.sw)?;
    t.raw_set("sh", f.sh)?;
    t.raw_set("dt", f.dt)?;
    t.raw_set("frame", f.frame)?;
    Ok(())
}
