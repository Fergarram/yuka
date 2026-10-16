//
// Lua boundary
//
// The scripts are where yuka's behaviour lives: what a key does, what a gesture means,
// where a scroll goes, how the view moves, which window gets focus. The engine runs
// them, hands them a snapshot of the frame, and takes back a list of commands. Nothing
// in here decides anything.
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

use mlua::{Function, IntoLua, Lua, Table, Value};

use crate::ray;
use crate::render::WinList;
use crate::settings::{self, Settings};

//
// Constants
//

// Where the scripts live, unless YUKA_LUA says otherwise.
pub const DEFAULT_DIR: &str = "lua";
// The entry script, inside that directory.
const ENTRY: &str = "init.lua";
// How many entries a snapshot list is expected to carry. The tables are sized for this
// once and grow if a frame ever has more.
const LIST_HINT: usize = 32;
// Same count of keycodes input.rs tracks.
const KEY_CODES: usize = crate::input::KEY_CODES;

//
// Types
//

// Debug overlays a script can switch.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Debug {
    Labels,
    Pad,
}

// What a script can ask the engine to do. Drained in order after each call.
#[derive(Clone, PartialEq, Debug)]
pub enum Cmd {
    // Re-read the scripts, as a save would.
    Reload,
    ToggleDebug(Debug),
    // Ask a window to close. Handles are render::Windows ids.
    Close(u64),
    Screenshot,
    // Put the view here: canvas centre and zoom. The zoom is held to the engine's limits.
    SetCamera(f32, f32, f32),
    // Give the keyboard to a window, or to nothing.
    Focus(Option<u64>),
    // Bring a window to the top of the stack.
    Front(u64),
    // This frame's left press is the script's, and goes no further: no drag starts from it
    // and no client hears it.
    EatPress,
    // Let go of every window: keyboard, pointer and implicit grab.
    ReleaseAll,
    // Pick a window up at the pointer, and whether it lifts toward the camera. Consumes the
    // press. Follows the pointer until the left button comes up.
    BeginMove(u64, bool),
    // Resize a window from the pointer, holding a corner: per axis, 1 for the far edge (right,
    // bottom) and -1 for the near one. Follows until the right button comes up.
    BeginResize(u64, f32, f32),
    // Change which corner the resize in progress holds.
    ResizeCorner(f32, f32),
    // How opaque a window should be this frame, eased toward. Every window not named is opaque.
    WindowAlpha(u64, f32),
    // The colour behind the canvas, as 0xRRGGBBAA.
    Background(u32),
    // Start a program as a client of this compositor.
    Spawn(String),
}

// Where this frame's scrolling and pinching go. true means the client under the pointer,
// false means nobody: what the canvas does with it is the script's own business.
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
    // Pointer motion this frame, the left and middle button edges, the wheels and the middle
    // button level.
    pub dx: f32,
    pub dy: f32,
    pub pressed: bool,
    pub middle_pressed: bool,
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
    // Three fingers: on the pad, how far toward firing (1 at the trigger), which way as a unit
    // vector in screen sense, fired on lift, and tapped twice.
    pub swiping: bool,
    pub swipe_progress: f32,
    pub swipe_dir: (f32, f32),
    pub swipe_fired: bool,
    pub tap3: bool,
    // Window handles, or None. Under is the window beneath the pointer.
    pub focused: Option<u64>,
    pub hovered: Option<u64>,
    pub under: Option<u64>,
    // A window is being moved by a drag, the right button went down, and the resize in
    // progress, ours rather than a client's: window and the corner it holds.
    pub dragging: bool,
    pub right_pressed: bool,
    pub resizing: Option<(u64, f32, f32)>,
    // The view, the screen, and the clock.
    pub zoom: f32,
    pub cx: f32,
    pub cy: f32,
    pub sw: f32,
    pub sh: f32,
    pub dt: f32,
    pub time_ms: u32,
    pub frame: u32,
    // Engine settings the scripts also need to know.
    pub zoom_min: f32,
    pub zoom_max: f32,
    pub zoom_default: f32,
    pub double_click_ms: u32,
    // Every toplevel on the canvas.
    pub windows: &'a WinList,
    // Windows that drew their first frame, with whether each came from the client that has
    // the keyboard (or nothing had it), and windows another client asked to activate.
    pub mapped: &'a [(u64, bool)],
    pub activated: &'a [u64],
}

// What the functions a script calls write into. Lives in the VM's app data, so the
// callbacks need no shared ownership to reach it.
struct Io {
    out: Vec<Cmd>,
    route: Route,
    held: Vec<bool>,
    // Inside yk.draw, which is the only time the drawing functions may touch raylib: outside
    // the draw pass a draw call would land in the middle of the engine's own GL state.
    drawing: bool,
}

// The list tables inside the snapshot, kept so a frame refills them rather than making new
// ones.
struct Lists {
    key_code: Table,
    key_press: Table,
    win_id: Table,
    win_x: Table,
    win_y: Table,
    win_w: Table,
    win_h: Table,
    win_max: Table,
    win_order: Table,
    mapped: Table,
    mapped_welcome: Table,
    activated: Table,
}

// One loaded script set: the VM and the handles the frame call reuses, so a frame
// allocates no tables.
struct Vm {
    lua: Lua,
    frame_fn: Function,
    draw_fn: Option<Function>,
    snap: Table,
    lists: Lists,
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
        drawing: false,
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
    let draw_fn: Option<Function> = match yk.get::<Value>("draw")? {
        Value::Function(f) => Some(f),
        _ => None,
    };
    let pairs = match yk.get::<Value>("config")? {
        Value::Table(t) => config_pairs(&t)?,
        Value::Nil => Vec::new(),
        _ => return Err(mlua::Error::RuntimeError("yk.config is not a table".into())),
    };

    let snap = lua.create_table_with_capacity(0, 64)?;
    let list = |name: &str| -> mlua::Result<Table> {
        let t = lua.create_table_with_capacity(LIST_HINT, 0)?;
        snap.raw_set(name, t.clone())?;
        Ok(t)
    };
    let lists = Lists {
        key_code: list("key_code")?,
        key_press: list("key_press")?,
        win_id: list("win_id")?,
        win_x: list("win_x")?,
        win_y: list("win_y")?,
        win_w: list("win_w")?,
        win_h: list("win_h")?,
        win_max: list("win_max")?,
        win_order: list("win_order")?,
        mapped: list("mapped")?,
        mapped_welcome: list("mapped_welcome")?,
        activated: list("activated")?,
    };
    Ok((Vm { lua, frame_fn, draw_fn, snap, lists }, pairs))
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

// An edge direction as the engine wants it: exactly -1 or 1.
fn edge(v: f32) -> f32 {
    if v < 0.0 { -1.0 } else { 1.0 }
}

fn debug_named(name: &str) -> mlua::Result<Debug> {
    match name {
        "labels" => Ok(Debug::Labels),
        "pad" => Ok(Debug::Pad),
        _ => Err(mlua::Error::RuntimeError(format!(
            "no debug overlay called '{name}' (labels, pad)"
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
    yk.set("set_camera", lua.create_function(|lua, (cx, cy, zoom): (f32, f32, f32)| {
        push(lua, Cmd::SetCamera(cx, cy, zoom));
        Ok(())
    })?)?;
    yk.set("focus", lua.create_function(|lua, id: Option<u64>| {
        push(lua, Cmd::Focus(id));
        Ok(())
    })?)?;
    yk.set("front", lua.create_function(|lua, id: Option<u64>| {
        if let Some(id) = id {
            push(lua, Cmd::Front(id));
        }
        Ok(())
    })?)?;
    yk.set("eat_press", lua.create_function(|lua, ()| {
        push(lua, Cmd::EatPress);
        Ok(())
    })?)?;
    yk.set("release_all", lua.create_function(|lua, ()| {
        push(lua, Cmd::ReleaseAll);
        Ok(())
    })?)?;
    yk.set("begin_move", lua.create_function(|lua, (id, lift): (u64, bool)| {
        push(lua, Cmd::BeginMove(id, lift));
        Ok(())
    })?)?;
    yk.set("begin_resize", lua.create_function(|lua, (id, ex, ey): (u64, f32, f32)| {
        push(lua, Cmd::BeginResize(id, edge(ex), edge(ey)));
        Ok(())
    })?)?;
    yk.set("resize_corner", lua.create_function(|lua, (ex, ey): (f32, f32)| {
        push(lua, Cmd::ResizeCorner(edge(ex), edge(ey)));
        Ok(())
    })?)?;
    yk.set("window_alpha", lua.create_function(|lua, (id, alpha): (u64, f32)| {
        push(lua, Cmd::WindowAlpha(id, alpha));
        Ok(())
    })?)?;
    yk.set("set_background", lua.create_function(|lua, rgba: u32| {
        push(lua, Cmd::Background(rgba));
        Ok(())
    })?)?;
    register_draw(lua, yk)?;
    yk.set("spawn", lua.create_function(|lua, cmd: String| {
        push(lua, Cmd::Spawn(cmd));
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
// Drawing
//
// Thin wrappers over raylib's 2D calls, in screen pixels, with colours as 0xRRGGBBAA. Only
// inside yk.draw, which the engine calls once a frame after the windows are drawn.
//

fn color(rgba: u32) -> ray::Color {
    ray::Color {
        r: (rgba >> 24) as u8,
        g: (rgba >> 16) as u8,
        b: (rgba >> 8) as u8,
        a: rgba as u8,
    }
}

fn check_drawing(lua: &Lua) -> mlua::Result<()> {
    let drawing = lua.app_data_ref::<Io>().map_or(false, |io| io.drawing);
    if drawing {
        Ok(())
    } else {
        Err(mlua::Error::RuntimeError("drawing outside yk.draw".into()))
    }
}

fn register_draw(lua: &Lua, yk: &Table) -> mlua::Result<()> {
    yk.set("draw_rect", lua.create_function(|lua, (x, y, w, h, c): (f32, f32, f32, f32, u32)| {
        check_drawing(lua)?;
        ray::draw_rectangle(x as i32, y as i32, w as i32, h as i32, color(c));
        Ok(())
    })?)?;
    yk.set("draw_rect_lines", lua.create_function(|lua, (x, y, w, h, c): (f32, f32, f32, f32, u32)| {
        check_drawing(lua)?;
        ray::draw_rectangle_lines(x as i32, y as i32, w as i32, h as i32, color(c));
        Ok(())
    })?)?;
    yk.set("draw_line", lua.create_function(|lua, (x1, y1, x2, y2, c): (f32, f32, f32, f32, u32)| {
        check_drawing(lua)?;
        ray::draw_line(x1 as i32, y1 as i32, x2 as i32, y2 as i32, color(c));
        Ok(())
    })?)?;
    yk.set("draw_circle", lua.create_function(|lua, (x, y, r, c): (f32, f32, f32, u32)| {
        check_drawing(lua)?;
        ray::draw_circle(x as i32, y as i32, r, color(c));
        Ok(())
    })?)?;
    yk.set("draw_text", lua.create_function(|lua, (text, x, y, size, c): (String, f32, f32, i32, u32)| {
        check_drawing(lua)?;
        ray::draw_text(&text, x as i32, y as i32, size, color(c));
        Ok(())
    })?)?;
    yk.set("measure_text", lua.create_function(|_, (text, size): (String, i32)| {
        Ok(ray::measure_text(&text, size))
    })?)?;
    Ok(())
}

// Run yk.draw, if the scripts define one. Called inside the draw pass, after the windows and
// the engine's own instruments.
pub fn draw(s: &mut Script) {
    let vm = &s.vm;
    let Some(draw_fn) = vm.draw_fn.as_ref() else { return };
    if let Some(mut io) = vm.lua.app_data_mut::<Io>() {
        io.drawing = true;
    }
    let result = draw_fn.call::<()>(());
    if let Some(mut io) = vm.lua.app_data_mut::<Io>() {
        io.drawing = false;
    }
    report(s, "draw", result);
}

// Keep a call's error for the screen, once per distinct error, and clear it when that call
// runs cleanly again. A failed reload is not cleared here: it stands until the scripts load.
fn report(s: &mut Script, what: &str, result: mlua::Result<()>) {
    let prefix = format!("{what}: ");
    match result {
        Ok(()) => {
            if s.error.as_deref().is_some_and(|e| e.starts_with(&prefix)) {
                s.error = None;
            }
        }
        Err(e) => {
            let msg = format!("{prefix}{e}");
            if s.error.as_deref() != Some(msg.as_str()) {
                eprintln!("yuka: lua: {msg}");
                s.error = Some(msg);
            }
        }
    }
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
        out.extend(io.out.drain(..));
        route = io.route;
    }
    report(s, "frame", result);
    route
}

// Entries 1..=n of a list table. Entries past n from a longer earlier frame are left
// alone: every list comes with its count, and nothing reads past it.
fn fill<T: IntoLua + Copy>(t: &Table, values: &[T]) -> mlua::Result<()> {
    for (i, v) in values.iter().enumerate() {
        t.raw_set(i + 1, *v)?;
    }
    Ok(())
}

fn fill_snapshot(vm: &Vm, f: &FrameIn) -> mlua::Result<()> {
    let t = &vm.snap;
    let l = &vm.lists;

    for (i, &(code, pressed)) in f.keys.iter().enumerate() {
        l.key_code.raw_set(i + 1, code)?;
        l.key_press.raw_set(i + 1, pressed)?;
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
    t.raw_set("pressed", f.pressed)?;
    t.raw_set("middle_pressed", f.middle_pressed)?;
    t.raw_set("wheel", f.wheel)?;
    t.raw_set("hwheel", f.hwheel)?;
    t.raw_set("middle", f.middle)?;
    t.raw_set("scroll_x", f.scroll_x)?;
    t.raw_set("scroll_y", f.scroll_y)?;
    t.raw_set("scroll_lifted", f.scroll_lifted)?;
    t.raw_set("pad_driving", f.pad_driving)?;
    t.raw_set("fingers", f.fingers)?;
    t.raw_set("pinch", f.pinch)?;
    t.raw_set("swiping", f.swiping)?;
    t.raw_set("swipe_progress", f.swipe_progress)?;
    t.raw_set("swipe_dx", f.swipe_dir.0)?;
    t.raw_set("swipe_dy", f.swipe_dir.1)?;
    t.raw_set("swipe_fired", f.swipe_fired)?;
    t.raw_set("tap3", f.tap3)?;

    t.raw_set("focused", f.focused)?;
    t.raw_set("hovered", f.hovered)?;
    t.raw_set("under", f.under)?;
    t.raw_set("dragging", f.dragging)?;
    t.raw_set("right_pressed", f.right_pressed)?;
    match f.resizing {
        Some((id, ex, ey)) => {
            t.raw_set("resizing", id)?;
            t.raw_set("resize_ex", ex)?;
            t.raw_set("resize_ey", ey)?;
        }
        None => {
            t.raw_set("resizing", Value::Nil)?;
            t.raw_set("resize_ex", 0)?;
            t.raw_set("resize_ey", 0)?;
        }
    }

    t.raw_set("zoom", f.zoom)?;
    t.raw_set("cx", f.cx)?;
    t.raw_set("cy", f.cy)?;
    t.raw_set("sw", f.sw)?;
    t.raw_set("sh", f.sh)?;
    t.raw_set("dt", f.dt)?;
    t.raw_set("time_ms", f.time_ms)?;
    t.raw_set("frame", f.frame)?;
    t.raw_set("zoom_min", f.zoom_min)?;
    t.raw_set("zoom_max", f.zoom_max)?;
    t.raw_set("zoom_default", f.zoom_default)?;
    t.raw_set("double_click_ms", f.double_click_ms)?;

    let w = f.windows;
    t.raw_set("win_count", w.id.len())?;
    fill(&l.win_id, &w.id)?;
    fill(&l.win_x, &w.x)?;
    fill(&l.win_y, &w.y)?;
    fill(&l.win_w, &w.w)?;
    fill(&l.win_h, &w.h)?;
    fill(&l.win_max, &w.maximized)?;
    fill(&l.win_order, &w.order)?;

    t.raw_set("mapped_count", f.mapped.len())?;
    for (i, &(id, welcome)) in f.mapped.iter().enumerate() {
        l.mapped.raw_set(i + 1, id)?;
        l.mapped_welcome.raw_set(i + 1, welcome)?;
    }
    t.raw_set("activated_count", f.activated.len())?;
    fill(&l.activated, f.activated)?;
    Ok(())
}
