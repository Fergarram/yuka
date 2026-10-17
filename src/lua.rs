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
use crate::render::{Inspect, WinList};
use crate::touch::PadView;
use crate::settings::{self, Settings};

//
// Constants
//

// Where the scripts live, unless YUKA_LUA says otherwise.
pub const DEFAULT_DIR: &str = "lua";
// Where window shaders live, watched alongside the scripts.
const SHADER_DIR: &str = "shaders";
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

// What a script can ask the engine to do. Drained in order after each call.
#[derive(Clone, PartialEq, Debug)]
pub enum Cmd {
    // Re-read the scripts, as a save would.
    Reload,
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
    // Pick a window up and have it follow the pointer until the left button comes up. Held at
    // the screen point given, or at the pointer. from_client is a move the client asked for
    // (its titlebar): the client then keeps its press and gets its release, and the stack is
    // left alone. Ours consumes the press and brings the window to the front.
    BeginMove { id: u64, anchor: Option<(f32, f32)>, from_client: bool },
    // Resize a window from the pointer, holding a corner: per axis, 1 for the far edge (right,
    // bottom) and -1 for the near one. Follows until the right button comes up.
    BeginResize(u64, f32, f32),
    // Change which corner the resize in progress holds.
    ResizeCorner(f32, f32),
    // How a window looks: opacity, lift toward the camera (negative z is nearer), whether its
    // shadow padding is drawn, and whether it is stretched to the size a resize asks for. Kept until changed; popups and subsurfaces follow their
    // parent.
    WindowLook { id: u64, alpha: f32, z: f32, shadow: bool, stretch: bool },
    // The colour behind the canvas, as 0xRRGGBBAA.
    Background(u32),
    // Ask a window to be this size, in canvas units. Clamped to the limits the client set.
    ResizeWindow(u64, f32, f32),
    // Put a window's visible top-left at a canvas point.
    MoveWindow(u64, f32, f32),
    // Fill a canvas rectangle with a window, remembering what to give back, and tell the
    // client. Or give it back. Answers a maximize request; also works unasked.
    Maximize(u64, f32, f32, f32, f32),
    Unmaximize(u64),
    // The same for fullscreen: fill a canvas rectangle and tell the client it is fullscreen, or
    // give it back.
    Fullscreen(u64, f32, f32, f32, f32),
    Unfullscreen(u64),
    // Take a window out of drawing and hit testing, children included, or bring it back. It
    // stays in the snapshot.
    Hide(u64, bool),
    // Load a window shader into a slot (0 is the built-in one), from a vertex and a fragment
    // file. One that will not compile leaves the slot as it was.
    LoadShader { slot: u8, vs: String, fs: String },
    // Set a uniform on every window a slot draws: one to four floats.
    ShaderUniform { slot: u8, name: String, values: [f32; 4], count: u8 },
    // Which slot draws a window, and four numbers for it (windowParams).
    WindowShader { id: u64, slot: u8, params: [f32; 4] },
    // Whether clients draw their own shadows; off tells every window it is tiled.
    ClientShadows(bool),
    // Whether clients draw their own titlebars; off tells every window the compositor
    // decorates it, and we draw nothing.
    ClientDecorations(bool),
    // The canvas rectangle popups (menus, tooltips) are kept inside, as x, y, w, h, or None to
    // put them exactly where their client asks. Kept until changed.
    PopupBounds(Option<[f32; 4]>),
    // Which shader slot draws the floor, the canvas plane behind every window, or no floor.
    FloorShader(Option<u8>),
    // The cursor shown when no client has the pointer, by theme name ("default", "grabbing",
    // "nwse-resize"...).
    Cursor(String),
    // Start a program as a client of this compositor.
    Spawn(String),
    // A new keymap from xkb names, each a comma separated list as setxkbmap takes it: layouts,
    // their variants, and options. Applied only when it differs from the one in force, so the
    // scripts can ask on every load. One xkb will not compile keeps the old keymap.
    SetLayouts { layout: String, variant: String, options: String },
    // Make a layout of the keymap the active one, by index from 0.
    SetLayout(u32),
}

// What reaches clients this frame. true means the client under the pointer (or, for keys,
// the one with the keyboard); false means nobody, and what the canvas does with it instead is
// the script's own business.
//
// Only the destination. The protocol rules that come with sending to a client (axis
// sequences that must be stopped, pinch sequences that must be ended, releases that must
// follow their presses) stay in the engine, so a script cannot leave a client believing a
// gesture is still running or a key still held.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct Route {
    pub wheel: bool,
    pub pinch: bool,
    // A finger scroll for the client under the pointer this frame, in the client's own units
    // and sign, already shaped by the scripts (sensitivity, axis lock). None sends nothing.
    pub fingers: Option<(f32, f32)>,
    // Hover, motion and buttons.
    pub pointer: bool,
    // Key presses. Modifiers and releases go regardless.
    pub keys: bool,
}

// What a window is called and whose it is, sent when it is created, when any of it changes, and
// for every window after the scripts reload. Strings, so never every frame.
pub struct WinInfo {
    pub id: u64,
    pub app_id: String,
    pub title: String,
    // The window this one belongs to, for a dialog.
    pub parent: Option<u64>,
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
    // The fingers making a scroll have lifted, as libinput reports it, and the scroll has
    // stopped for any reason (lifted, or turned into something else), which ends a sequence.
    pub scroll_lifted: bool,
    pub scroll_ended: bool,
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
    // The window being moved by a drag, the right button went down, and the resize in
    // progress, ours rather than a client's: window and the corner it holds.
    pub dragging: Option<u64>,
    pub right_pressed: bool,
    pub resizing: Option<(u64, f32, f32)>,
    // Any resize in progress, ours or a client's edge drag.
    pub stretching: Option<u64>,
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
    // How far the camera sits from the canvas plane, in canvas units, for projecting a lifted
    // window: a point at height z shows at zoom * dist / (dist + z).
    pub cam_dist: f32,
    // Every toplevel on the canvas.
    pub windows: &'a WinList,
    // Windows that drew their first frame, with whether each came from the client that has
    // the keyboard (or nothing had it), and windows another client asked to activate.
    pub mapped: &'a [(u64, bool)],
    // Windows a client created since the last frame, not drawn yet. Their first configure goes
    // out after this frame's commands, so a yk.resize_window now is the size they first draw at.
    pub created: &'a [u64],
    pub activated: &'a [u64],
    // Windows asking to be maximized (true) or let go of it (false). A request the script
    // does not answer with yk.maximize or yk.unmaximize is answered for it: refused, or the
    // window's own size back.
    pub max_requests: &'a [(u64, bool)],
    // And fullscreen. Unanswered ones are refused, or given their own size back.
    pub full_requests: &'a [(u64, bool)],
    // Windows whose app id, title or parent the scripts have not heard yet.
    pub info: &'a [WinInfo],
    // Clients asking to be moved by their titlebar, with the screen point the press landed on.
    // A request the script does not accept with yk.accept_move is ignored.
    pub move_requests: &'a [(u64, f32, f32)],
    // Windows that closed since the last frame.
    pub closed: &'a [u64],
    // Every contact on the trackpad as the gesture code sees it, when there is a pad.
    pub touches: Option<&'a PadView>,
    // The keymap's layouts by full name, the active one by index from 0, and a counter bumped
    // whenever the names change, so they are copied into the VM only then.
    pub layout: u32,
    pub layout_names: &'a [String],
    pub layouts_gen: u32,
    // An input method has the keyboard: a text field has focus and something like fcitx5 is
    // reading the keys for it.
    pub ime: bool,
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
    // Engine data the scripts asked to draw this frame (yk.want_inspect, yk.want_pad), and the
    // data itself while yk.draw runs.
    want_inspect: bool,
    want_pad: bool,
    inspect: Vec<Inspect>,
    pad: Option<PadView>,
    cam3d: Option<ray::Camera3D>,
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
    win_full: Table,
    win_hidden: Table,
    win_order: Table,
    win_z: Table,
    win_rw: Table,
    win_rh: Table,
    win_gw: Table,
    win_gh: Table,
    move_id: Table,
    move_x: Table,
    move_y: Table,
    closed: Table,
    touch_x: Table,
    touch_y: Table,
    touch_size: Table,
    touch_resting: Table,
    touch_faint: Table,
    mapped: Table,
    mapped_welcome: Table,
    created: Table,
    activated: Table,
    max_id: Table,
    max_on: Table,
    full_id: Table,
    full_on: Table,
    info_id: Table,
    info_app_id: Table,
    info_title: Table,
    info_parent: Table,
    layout_names: Table,
}

// One loaded script set: the VM and the handles the frame call reuses, so a frame
// allocates no tables.
struct Vm {
    lua: Lua,
    frame_fn: Function,
    draw_fn: Option<Function>,
    snap: Table,
    lists: Lists,
    // Which FrameIn::layouts_gen the layout names in the snapshot are from. 0 is none yet.
    layouts_gen: u32,
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
        want_inspect: false,
        want_pad: false,
        inspect: Vec::new(),
        pad: None,
        cam3d: None,
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
        win_full: list("win_full")?,
        win_hidden: list("win_hidden")?,
        win_order: list("win_order")?,
        win_z: list("win_z")?,
        win_rw: list("win_rw")?,
        win_rh: list("win_rh")?,
        win_gw: list("win_gw")?,
        win_gh: list("win_gh")?,
        move_id: list("move_id")?,
        move_x: list("move_x")?,
        move_y: list("move_y")?,
        closed: list("closed")?,
        touch_x: list("touch_x")?,
        touch_y: list("touch_y")?,
        touch_size: list("touch_size")?,
        touch_resting: list("touch_resting")?,
        touch_faint: list("touch_faint")?,
        mapped: list("mapped")?,
        mapped_welcome: list("mapped_welcome")?,
        created: list("created")?,
        activated: list("activated")?,
        max_id: list("max_id")?,
        max_on: list("max_on")?,
        full_id: list("full_id")?,
        full_on: list("full_on")?,
        info_id: list("info_id")?,
        info_app_id: list("info_app_id")?,
        info_title: list("info_title")?,
        info_parent: list("info_parent")?,
        layout_names: list("layout_names")?,
    };
    Ok((Vm { lua, frame_fn, draw_fn, snap, lists, layouts_gen: 0 }, pairs))
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

// Newest modification time of the scripts and of the shaders they load, so saving either
// reloads: a reload recompiles every shader the scripts ask for.
fn newest_mtime(dir: &str) -> Option<SystemTime> {
    let mut newest: Option<SystemTime> = None;
    for d in [dir, SHADER_DIR] {
        let Ok(entries) = std::fs::read_dir(d) else { continue };
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            let Ok(t) = meta.modified() else { continue };
            if newest.map_or(true, |n| t > n) {
                newest = Some(t);
            }
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

fn register(lua: &Lua, yk: &Table) -> mlua::Result<()> {
    yk.set("reload", lua.create_function(|lua, ()| {
        push(lua, Cmd::Reload);
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
    yk.set("begin_move", lua.create_function(|lua, id: u64| {
        push(lua, Cmd::BeginMove { id, anchor: None, from_client: false });
        Ok(())
    })?)?;
    yk.set("accept_move", lua.create_function(|lua, (id, x, y): (u64, f32, f32)| {
        push(lua, Cmd::BeginMove { id, anchor: Some((x, y)), from_client: true });
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
    yk.set("window_look", lua.create_function(|lua, (id, alpha, z, shadow, stretch): (u64, f32, f32, bool, Option<bool>)| {
        push(lua, Cmd::WindowLook { id, alpha, z, shadow, stretch: stretch.unwrap_or(false) });
        Ok(())
    })?)?;
    yk.set("want_inspect", lua.create_function(|lua, ()| {
        if let Some(mut io) = lua.app_data_mut::<Io>() {
            io.want_inspect = true;
        }
        Ok(())
    })?)?;
    yk.set("want_pad", lua.create_function(|lua, ()| {
        if let Some(mut io) = lua.app_data_mut::<Io>() {
            io.want_pad = true;
        }
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
    yk.set("route_scroll", lua.create_function(|lua, (wheel, pinch): (bool, bool)| {
        if let Some(mut io) = lua.app_data_mut::<Io>() {
            io.route.wheel = wheel;
            io.route.pinch = pinch;
        }
        Ok(())
    })?)?;
    yk.set("scroll_client", lua.create_function(|lua, (h, v): (f32, f32)| {
        if let Some(mut io) = lua.app_data_mut::<Io>() {
            io.route.fingers = Some((h, v));
        }
        Ok(())
    })?)?;
    yk.set("shader", lua.create_function(|lua, (slot, vs, fs): (u8, String, String)| {
        push(lua, Cmd::LoadShader { slot, vs, fs });
        Ok(())
    })?)?;
    yk.set("shader_uniform", lua.create_function(
        |lua, (slot, name, a, b, c, d): (u8, String, f32, Option<f32>, Option<f32>, Option<f32>)| {
            let count = 1 + b.is_some() as u8 + c.is_some() as u8 + d.is_some() as u8;
            let values = [a, b.unwrap_or(0.0), c.unwrap_or(0.0), d.unwrap_or(0.0)];
            push(lua, Cmd::ShaderUniform { slot, name, values, count });
            Ok(())
        },
    )?)?;
    yk.set("window_shader", lua.create_function(
        |lua, (id, slot, a, b, c, d): (u64, u8, Option<f32>, Option<f32>, Option<f32>, Option<f32>)| {
            let params = [a.unwrap_or(0.0), b.unwrap_or(0.0), c.unwrap_or(0.0), d.unwrap_or(0.0)];
            push(lua, Cmd::WindowShader { id, slot, params });
            Ok(())
        },
    )?)?;
    yk.set("client_shadows", lua.create_function(|lua, on: bool| {
        push(lua, Cmd::ClientShadows(on));
        Ok(())
    })?)?;
    yk.set("client_decorations", lua.create_function(|lua, on: bool| {
        push(lua, Cmd::ClientDecorations(on));
        Ok(())
    })?)?;
    yk.set("popup_bounds", lua.create_function(
        |lua, (x, y, w, h): (Option<f32>, Option<f32>, Option<f32>, Option<f32>)| {
            let rect = match (x, y, w, h) {
                (Some(x), Some(y), Some(w), Some(h)) => Some([x, y, w, h]),
                _ => None,
            };
            push(lua, Cmd::PopupBounds(rect));
            Ok(())
        },
    )?)?;
    yk.set("floor_shader", lua.create_function(|lua, slot: Option<u8>| {
        push(lua, Cmd::FloorShader(slot));
        Ok(())
    })?)?;
    yk.set("set_cursor", lua.create_function(|lua, name: String| {
        push(lua, Cmd::Cursor(name));
        Ok(())
    })?)?;
    yk.set("route_input", lua.create_function(|lua, (pointer, keys): (bool, bool)| {
        if let Some(mut io) = lua.app_data_mut::<Io>() {
            io.route.pointer = pointer;
            io.route.keys = keys;
        }
        Ok(())
    })?)?;
    yk.set("move_window", lua.create_function(|lua, (id, x, y): (u64, f32, f32)| {
        push(lua, Cmd::MoveWindow(id, x, y));
        Ok(())
    })?)?;
    yk.set("fullscreen", lua.create_function(|lua, (id, x, y, w, h): (u64, f32, f32, f32, f32)| {
        push(lua, Cmd::Fullscreen(id, x, y, w, h));
        Ok(())
    })?)?;
    yk.set("unfullscreen", lua.create_function(|lua, id: u64| {
        push(lua, Cmd::Unfullscreen(id));
        Ok(())
    })?)?;
    yk.set("hide", lua.create_function(|lua, (id, on): (u64, Option<bool>)| {
        push(lua, Cmd::Hide(id, on.unwrap_or(true)));
        Ok(())
    })?)?;
    yk.set("resize_window", lua.create_function(|lua, (id, w, h): (u64, f32, f32)| {
        push(lua, Cmd::ResizeWindow(id, w, h));
        Ok(())
    })?)?;
    yk.set("maximize", lua.create_function(|lua, (id, x, y, w, h): (u64, f32, f32, f32, f32)| {
        push(lua, Cmd::Maximize(id, x, y, w, h));
        Ok(())
    })?)?;
    yk.set("unmaximize", lua.create_function(|lua, id: u64| {
        push(lua, Cmd::Unmaximize(id));
        Ok(())
    })?)?;
    // Layouts are 1 based here like every Lua list, and from 0 in the engine.
    yk.set("set_layouts", lua.create_function(
        |lua, (layout, variant, options): (String, Option<String>, Option<String>)| {
            push(lua, Cmd::SetLayouts {
                layout,
                variant: variant.unwrap_or_default(),
                options: options.unwrap_or_default(),
            });
            Ok(())
        },
    )?)?;
    yk.set("set_layout", lua.create_function(|lua, i: u32| {
        if i >= 1 {
            push(lua, Cmd::SetLayout(i - 1));
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
    yk.set("draw_ellipse_lines", lua.create_function(|lua, (x, y, rx, ry, c): (f32, f32, f32, f32, u32)| {
        check_drawing(lua)?;
        ray::draw_ellipse_lines(x as i32, y as i32, rx, ry, color(c));
        Ok(())
    })?)?;
    // Where a canvas point at a lift z lands on screen, through the camera the windows were
    // just drawn with. Only inside yk.draw.
    yk.set("to_screen", lua.create_function(|lua, (x, y, z): (f32, f32, f32)| {
        check_drawing(lua)?;
        let cam = lua.app_data_ref::<Io>().and_then(|io| io.cam3d);
        let Some(cam) = cam else { return Ok((0.0, 0.0)) };
        let v = ray::world_to_screen(ray::Vector3 { x, y, z }, cam);
        Ok((v.x, v.y))
    })?)?;
    yk.set("draw_text", lua.create_function(|lua, (text, x, y, size, c): (String, f32, f32, i32, u32)| {
        check_drawing(lua)?;
        ray::draw_text(&text, x as i32, y as i32, size, color(c));
        Ok(())
    })?)?;
    yk.set("measure_text", lua.create_function(|_, (text, size): (String, i32)| {
        Ok(ray::measure_text(&text, size))
    })?)?;
    // Every drawable surface, children included, for the labels overlay. Only inside yk.draw on
    // a frame that called yk.want_inspect; empty otherwise. Builds tables, so it is for
    // instruments rather than for every frame.
    yk.set("surfaces", lua.create_function(|lua, ()| {
        check_drawing(lua)?;
        let list = lua.create_table()?;
        let Some(io) = lua.app_data_ref::<Io>() else { return Ok(list) };
        for (n, w) in io.inspect.iter().enumerate() {
            let t = lua.create_table_with_capacity(0, 17)?;
            t.raw_set("kind", w.kind)?;
            t.raw_set("source", w.source)?;
            t.raw_set("tex_id", w.tex_id)?;
            t.raw_set("tex_w", w.tex_w)?;
            t.raw_set("tex_h", w.tex_h)?;
            t.raw_set("sampling", w.sampling.as_str())?;
            t.raw_set("canvas_x", w.canvas_x)?;
            t.raw_set("canvas_y", w.canvas_y)?;
            t.raw_set("geo_x", w.geo_x)?;
            t.raw_set("geo_y", w.geo_y)?;
            t.raw_set("geo_w", w.geo_w)?;
            t.raw_set("geo_h", w.geo_h)?;
            t.raw_set("z", w.z)?;
            t.raw_set("x", w.x)?;
            t.raw_set("y", w.y)?;
            t.raw_set("w", w.w)?;
            t.raw_set("h", w.h)?;
            list.raw_set(n + 1, t)?;
        }
        Ok(list)
    })?)?;
    // The trackpad as the gesture code sees it, for the pad overlay. Only inside yk.draw on a
    // frame that called yk.want_pad, and nil when there is no pad.
    yk.set("pad", lua.create_function(|lua, ()| {
        check_drawing(lua)?;
        let Some(io) = lua.app_data_ref::<Io>() else { return Ok(Value::Nil) };
        let Some(p) = io.pad.as_ref() else { return Ok(Value::Nil) };
        let t = lua.create_table_with_capacity(0, 32)?;
        let fingers = lua.create_table_with_capacity(p.count, 0)?;
        for i in 0..p.count {
            let c = lua.create_table_with_capacity(0, 7)?;
            c.raw_set("x", p.fingers[i].0)?;
            c.raw_set("y", p.fingers[i].1)?;
            c.raw_set("major", p.size[i].0)?;
            c.raw_set("minor", p.size[i].1)?;
            c.raw_set("raw_major", p.major[i])?;
            c.raw_set("resting", p.resting[i])?;
            c.raw_set("faint", p.faint[i])?;
            fingers.raw_set(i + 1, c)?;
        }
        t.raw_set("fingers", fingers)?;
        t.raw_set("count", p.count)?;
        t.raw_set("active", p.active)?;
        t.raw_set("rest_zone", p.rest_zone)?;
        t.raw_set("contact_max", p.contact_max)?;
        t.raw_set("left", p.left)?;
        t.raw_set("right", p.right)?;
        t.raw_set("mode", p.mode)?;
        t.raw_set("pan_armed", p.pan_armed)?;
        t.raw_set("zoom_armed", p.zoom_armed)?;
        t.raw_set("ptr_armed", p.ptr_armed)?;
        t.raw_set("software_buttons", p.software_buttons)?;
        t.raw_set("strip", p.strip)?;
        t.raw_set("split", p.split)?;
        t.raw_set("aspect", p.aspect)?;
        t.raw_set("pan_x", p.pan.0)?;
        t.raw_set("pan_y", p.pan.1)?;
        t.raw_set("zoom", p.zoom)?;
        t.raw_set("has_size", p.has_size)?;
        t.raw_set("load", p.load)?;
        t.raw_set("load_max", p.load_max)?;
        Ok(Value::Table(t))
    })?)?;
    Ok(())
}

// What engine data the scripts want to draw this frame: (surface inspection, trackpad).
pub fn wants(s: &Script) -> (bool, bool) {
    s.vm.lua.app_data_ref::<Io>().map_or((false, false), |io| (io.want_inspect, io.want_pad))
}

// Run yk.draw, if the scripts define one. Called inside the draw pass, after the windows. The
// inspection list and the pad reading are lent for the call and handed back after.
pub fn draw(s: &mut Script, cam3d: ray::Camera3D, inspect: &mut Vec<Inspect>, pad: Option<PadView>) {
    let vm = &s.vm;
    let Some(draw_fn) = vm.draw_fn.as_ref() else { return };
    if let Some(mut io) = vm.lua.app_data_mut::<Io>() {
        io.drawing = true;
        std::mem::swap(&mut io.inspect, inspect);
        io.pad = pad;
        io.cam3d = Some(cam3d);
    }
    let result = draw_fn.call::<()>(());
    if let Some(mut io) = vm.lua.app_data_mut::<Io>() {
        io.drawing = false;
        std::mem::swap(&mut io.inspect, inspect);
        io.pad = None;
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
    let names_changed = s.vm.layouts_gen != f.layouts_gen;
    s.vm.layouts_gen = f.layouts_gen;
    let vm = &s.vm;

    if let Some(mut io) = vm.lua.app_data_mut::<Io>() {
        io.route = Route::default();
        io.want_inspect = false;
        io.want_pad = false;
        let n = f.held.len().min(io.held.len());
        io.held[..n].copy_from_slice(&f.held[..n]);
    }

    let result = fill_snapshot(vm, f, names_changed).and_then(|()| vm.frame_fn.call::<()>(vm.snap.clone()));

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

fn fill_snapshot(vm: &Vm, f: &FrameIn, names_changed: bool) -> mlua::Result<()> {
    let t = &vm.snap;
    let l = &vm.lists;

    t.raw_set("layout", f.layout + 1)?;
    t.raw_set("ime", f.ime)?;
    if names_changed {
        for (i, name) in f.layout_names.iter().enumerate() {
            l.layout_names.raw_set(i + 1, name.as_str())?;
        }
        t.raw_set("layout_count", f.layout_names.len())?;
    }

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
    t.raw_set("scroll_ended", f.scroll_ended)?;
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
    t.raw_set("stretching", f.stretching)?;
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
    t.raw_set("cam_dist", f.cam_dist)?;

    let w = f.windows;
    t.raw_set("win_count", w.id.len())?;
    fill(&l.win_id, &w.id)?;
    fill(&l.win_x, &w.x)?;
    fill(&l.win_y, &w.y)?;
    fill(&l.win_w, &w.w)?;
    fill(&l.win_h, &w.h)?;
    fill(&l.win_max, &w.maximized)?;
    fill(&l.win_full, &w.fullscreen)?;
    fill(&l.win_hidden, &w.hidden)?;
    fill(&l.win_order, &w.order)?;
    fill(&l.win_z, &w.z)?;
    fill(&l.win_rw, &w.restore_w)?;
    fill(&l.win_rh, &w.restore_h)?;
    fill(&l.win_gw, &w.geo_w)?;
    fill(&l.win_gh, &w.geo_h)?;
    t.raw_set("closed_count", f.closed.len())?;
    fill(&l.closed, f.closed)?;
    match f.touches {
        Some(p) => {
            t.raw_set("touch_count", p.count)?;
            for i in 0..p.count {
                l.touch_x.raw_set(i + 1, p.fingers[i].0)?;
                l.touch_y.raw_set(i + 1, p.fingers[i].1)?;
                l.touch_size.raw_set(i + 1, p.size[i].0)?;
                l.touch_resting.raw_set(i + 1, p.resting[i])?;
                l.touch_faint.raw_set(i + 1, p.faint[i])?;
            }
        }
        None => t.raw_set("touch_count", 0)?,
    }
    t.raw_set("move_count", f.move_requests.len())?;
    for (i, &(id, x, y)) in f.move_requests.iter().enumerate() {
        l.move_id.raw_set(i + 1, id)?;
        l.move_x.raw_set(i + 1, x)?;
        l.move_y.raw_set(i + 1, y)?;
    }

    t.raw_set("mapped_count", f.mapped.len())?;
    for (i, &(id, welcome)) in f.mapped.iter().enumerate() {
        l.mapped.raw_set(i + 1, id)?;
        l.mapped_welcome.raw_set(i + 1, welcome)?;
    }
    t.raw_set("created_count", f.created.len())?;
    fill(&l.created, f.created)?;
    t.raw_set("activated_count", f.activated.len())?;
    fill(&l.activated, f.activated)?;
    t.raw_set("full_count", f.full_requests.len())?;
    for (i, &(id, on)) in f.full_requests.iter().enumerate() {
        l.full_id.raw_set(i + 1, id)?;
        l.full_on.raw_set(i + 1, on)?;
    }
    t.raw_set("info_count", f.info.len())?;
    for (i, info) in f.info.iter().enumerate() {
        l.info_id.raw_set(i + 1, info.id)?;
        l.info_app_id.raw_set(i + 1, info.app_id.as_str())?;
        l.info_title.raw_set(i + 1, info.title.as_str())?;
        // 0 for none rather than nil, so the list has no holes.
        l.info_parent.raw_set(i + 1, info.parent.unwrap_or(0))?;
    }
    t.raw_set("max_count", f.max_requests.len())?;
    for (i, &(id, on)) in f.max_requests.iter().enumerate() {
        l.max_id.raw_set(i + 1, id)?;
        l.max_on.raw_set(i + 1, on)?;
    }
    Ok(())
}
