--
-- Looks
--
-- How the canvas looks, apart from the windows' own pixels: the colour behind them, and how
-- each window is drawn. Every frame every window is sent its look (yk.window_look):
-- how opaque, how far it is lifted toward the camera, and whether its shadow padding shows.
-- The animations live here too: fading a window that covers the one you are working in,
-- lifting a window you pick up, and setting it down again.
--

local view = require("view")
local windows = require("windows")

local looks = {}

--
-- Tuning
--

looks.tune = {
    -- Light or dark: "dark", "light", or "system", which asks gsettings
    -- (org.gnome.desktop.interface color-scheme) as the scripts load and falls back to
    -- theme_default when it states no preference, as it does until something sets it.
    -- looks.set_theme switches while running.
    theme = "system",
    theme_default = "light",

    -- What each theme paints, 0xRRGGBBAA: behind the canvas, and the floor's dots. On the dark
    -- background a dot alpha around 0x30 is a hint and 0x60 is plainly there.
    themes = {
        dark = { background = 0x181820ff, dots = 0xc8c8d040 },
        light = { background = 0xf2f2f5ff, dots = 0x30303a38 },
    },

    -- Whether windows draw their own drop shadows. false tells every window it is tiled, the one
    -- hint toolkits act on: GTK, libadwaita, Qt and Chromium then draw no shadow, square their
    -- corners, and offer no resize handles at their edges (Super+right drag still resizes). A
    -- client that ignores the hint keeps its shadow.
    client_shadows = false,

    -- The floor: a dot grid on the canvas itself, behind every window, moving and zooming
    -- with them. Drawn by shaders/grid.frag through slot 2 (shaders below); false for none.
    -- Its colour comes from the theme.
    --
    --   spacing   canvas units between dots, 24 is a dot every 24 pixels at zoom 1
    --   dot_px    dot size on screen, the same at any zoom
    --   fade_px   the closest dots get on screen: zoomed out past it, every other dot fades
    --             and the spacing doubles (24, 48, 96...), so the floor thins out instead of
    --             crowding into a pattern. Around 12 keeps it airy, 8 keeps it dense
    floor = { spacing = 24, dot_px = 2, fade_px = 12 },

    -- How opaque a window is drawn while it covers the one you are working in.
    --
    -- A window in front of the focused one hides work you are looking at. The usual answers
    -- are to raise the focused window or to move something, both of which rearrange the desk
    -- to solve a problem that only lasts while you are looking. Fading the thing on top
    -- instead leaves everything where it is and lets you see through it. Only windows above
    -- the focused one, and only where they actually overlap it.
    --
    -- 1.0 switches the whole idea off. Below about 0.2 the covering window gets hard to find.
    cover_opacity = 0.9,

    -- How fast opacity and lift move toward where they are going, as a fraction of the gap
    -- per second. Fast enough to feel immediate, slow enough that a click does not blink.
    fade_rate = 14.0,
    lift_rate = 14.0,

    -- How high a picked up window rises toward the camera, in canvas units.
    lift_height = 125.0,

    -- Whether to draw what a client padded around its window, which is where its shadow and
    -- its rounded corners live.
    --
    -- A toolkit commits a surface larger than the window it declares, and we cannot ask it to
    -- stop: the ones that do this do not implement server-side decorations. So the only choice
    -- is whether to show what is already in the buffer. false crops to the window, which
    -- slices the shadow off square and fills the rounded corners back in. true draws the whole
    -- surface, so shadows fall across whatever is behind them.
    --
    -- Only the drawing changes. Clicks, drags, placement and maximizing all still work on the
    -- window itself, so the padding can never be clicked. Per window: looks.shadow(f, i) below
    -- decides, and this is its answer for every window.
    shadows = true,

    -- Whether a resize stretches the window locally while the client catches up.
    --
    -- Off is the conventional Wayland behaviour: the window is whatever size the client last
    -- committed, so the corner reaches your cursor only once the client has answered. Nothing
    -- on screen is ever a size the window is not, which means no overshoot past a minimum the
    -- client will refuse, and no snap back when you let go.
    --
    -- On, the quad is scaled to your cursor while the client catches up, so the corner tracks
    -- your hand exactly and the contents are resampled until the client re-renders into them.
    resize_stretch = false,

    -- After a stretched resize is let go, how many frames to keep showing the size it ended on
    -- while waiting for the client to commit something new. A client that refuses the size
    -- (weston-editor does) would otherwise be shown stretched forever, and snapping back to
    -- the truth is better than lying about it indefinitely.
    stretch_hold_frames = 90,

    -- Shaders, by slot. Slot 0 is what draws every window unless something says otherwise;
    -- replace it to change how all of them look. Paths are from the directory yuka runs in,
    -- and saving a shader reloads it like a script. A shader that will not compile leaves its
    -- slot as it was and puts the error on screen. See src/render.rs (Shaders) for the
    -- uniforms every window shader is handed.
    shaders = {
        [1] = { "shaders/quad.vert", "shaders/desaturate.frag" },
        [2] = { "shaders/quad.vert", "shaders/grid.frag" },
    },

    -- How much colour drains out of windows you are not working in, through slot 1. 0 is off,
    -- and draws them all with slot 0 exactly as before; 1 is grey.
    unfocused_desaturate = 0.0,

    -- The cursor over the canvas, by theme name: when nothing in particular is happening,
    -- with Super held over a window (it can be picked up), while one is held, and while a
    -- resize holds each corner. "default" is our own arrow.
    cursor = "default",
    cursor_grab = "grab",
    cursor_held = "grabbing",
    cursor_resize = { nw = "nw-resize", ne = "ne-resize", sw = "sw-resize", se = "se-resize" },
}

local tune = looks.tune

--
-- State
--

-- Per window, by id: the drawn opacity and lift, and where each is heading.
local alpha, z, z_to = {}, {}, {}

-- The window picked up to be lifted, if any (set by looks.pick_up), and the one being dragged
-- last frame, to notice the drop.
local lifted = nil
local lifted_fresh = false
local was_dragging = nil

-- Windows easing back down after a drop, by id: the canvas centre and the lift at release.
-- The screen point under that centre is held while the lift comes down, so perspective does
-- not slide the window toward the middle of the screen.
local settling = {}

-- Released resizes still stretched while the client answers, by id: the size the client had
-- committed when it was let go, and how many frames it has waited.
local holding = {}
local was_stretching = nil
local stretch_seen_w, stretch_seen_h = 0, 0

-- Scratch, reused every frame: which windows are still here.
local seen = {}

--
-- Functions
--

-- Lift a window while it is held. Called when a drag starts.
function looks.pick_up(id)
    lifted = id
    lifted_fresh = true
end

-- Which shader slot draws a window and the four numbers handed to it (windowParams). Slot 0 and
-- nothing by default; this is the place to decide per window.
function looks.window_shader(f, i)
    if tune.unfocused_desaturate > 0 and f.win_id[i] ~= f.focused then
        return 1, tune.unfocused_desaturate
    end
    return 0, 0
end

-- Load every shader in the table into its slot. Run as the scripts load, so a reload
-- recompiles them.
for slot, files in pairs(tune.shaders) do
    yk.shader(slot, files[1], files[2])
end

--
-- Theme
--

-- The floor's numbers, handed to its shader as the scripts load and again when the theme
-- changes: a uniform stays set until the slot reloads, and the slot only reloads along with
-- this file.
local FLOOR_SLOT = 2

-- The theme in force, a key of tune.themes.
looks.theme = tune.theme_default

-- What gsettings says, "dark" or "light", or nil when it states no preference. Runs a process,
-- so only as the scripts load, never per frame.
local function system_theme()
    local p = io.popen("gsettings get org.gnome.desktop.interface color-scheme 2>/dev/null")
    if not p then return nil end
    local out = p:read("*a") or ""
    p:close()
    if string.find(out, "prefer%-dark") then return "dark" end
    if string.find(out, "prefer%-light") then return "light" end
    return nil
end

function looks.set_theme(name)
    if not tune.themes[name] then return end
    looks.theme = name
    if tune.floor then
        local c = tune.themes[name].dots
        yk.shader_uniform(FLOOR_SLOT, "dotColor",
            math.floor(c / 0x1000000) % 256 / 255, math.floor(c / 0x10000) % 256 / 255,
            math.floor(c / 0x100) % 256 / 255, c % 256 / 255)
    end
end

yk.client_shadows(tune.client_shadows)

if tune.floor then
    yk.shader_uniform(FLOOR_SLOT, "spacing", tune.floor.spacing)
    yk.shader_uniform(FLOOR_SLOT, "dotPx", tune.floor.dot_px)
    yk.shader_uniform(FLOOR_SLOT, "fadePx", tune.floor.fade_px)
end
if tune.theme == "system" then
    looks.set_theme(system_theme() or tune.theme_default)
else
    looks.set_theme(tune.theme)
end

-- The cursor over the canvas this frame, sent only when it changes.
local cursor_now = nil

local function canvas_cursor(f)
    local want = tune.cursor
    if f.resizing then
        local v = f.resize_ey < 0 and "n" or "s"
        local h = f.resize_ex < 0 and "w" or "e"
        want = tune.cursor_resize[v .. h] or want
    elseif f.dragging then
        want = tune.cursor_held
    elseif f.super and f.under then
        want = tune.cursor_grab
    end
    if want ~= cursor_now then
        cursor_now = want
        yk.set_cursor(want)
    end
end

-- Whether a window draws its shadow padding. One answer for all of them by default, but this
-- is the place to decide per window, by size or by whatever else the snapshot says.
function looks.shadow(f, i)
    return tune.shadows
end

local function ease(from, to, rate, dt)
    local t = math.min(rate * dt, 1)
    local v = from + (to - from) * t
    if math.abs(to - v) < 0.002 then return to end
    return v
end

-- Which windows cover the focused one, as a set of indices.
local covering = {}

local function find_covers(f)
    for k in pairs(covering) do covering[k] = nil end
    local fi = windows.index(f, f.focused)
    if not fi then return end
    local bx, by, bw, bh = f.win_x[fi], f.win_y[fi], f.win_w[fi], f.win_h[fi]
    for i = 1, f.win_count do
        if i ~= fi and f.win_order[i] > f.win_order[fi] then
            local ax, ay, aw, ah = f.win_x[i], f.win_y[i], f.win_w[i], f.win_h[i]
            if ax < bx + bw and bx < ax + aw and ay < by + bh and by < ay + ah then
                covering[i] = true
            end
        end
    end
end

-- A drag that ended sets its window down. Only a lifted window has anything to come down from.
local function notice_drop(f)
    if was_dragging and was_dragging ~= f.dragging then
        local id = was_dragging
        local i = windows.index(f, id)
        if i and z[id] and math.abs(z[id]) > 0.5 then
            local cx, cy = windows.center(f, i)
            settling[id] = { cx = cx, cy = cy, z0 = z[id] }
        end
        if lifted == id then lifted = nil end
    end
    if f.dragging then settling[f.dragging] = nil end
    -- A pick up whose drag never started (the engine starts it after this frame's scripts, so
    -- one frame of grace) is forgotten rather than left to lift some later drag.
    if lifted and f.dragging ~= lifted then
        if lifted_fresh then lifted_fresh = false else lifted = nil end
    else
        lifted_fresh = false
    end
    was_dragging = f.dragging
end

-- Whether a window is shown at the size a resize asks for: while the resize runs, and after it
-- until the client has committed something new, or the hold runs out, or something else takes
-- the window (a drag, maximizing).
local function stretched(f, i, id)
    if not tune.resize_stretch then
        holding[id] = nil
        return false
    end
    if f.stretching == id then return true end
    local h = holding[id]
    if not h then return false end
    h.frames = h.frames + 1
    local answered = f.win_gw[i] ~= h.gw or f.win_gh[i] ~= h.gh
    if answered or h.frames >= tune.stretch_hold_frames or f.dragging == id or f.win_max[i] then
        holding[id] = nil
        return false
    end
    return true
end

-- A resize that ended starts its hold, measured from what the client had committed on its last
-- frame.
local function notice_resize_end(f)
    if was_stretching and was_stretching ~= f.stretching then
        holding[was_stretching] = { gw = stretch_seen_w, gh = stretch_seen_h, frames = 0 }
    end
    was_stretching = f.stretching
    local i = windows.index(f, f.stretching)
    if i then
        stretch_seen_w, stretch_seen_h = f.win_gw[i], f.win_gh[i]
    end
end

function looks.frame(f)
    yk.set_background(tune.themes[looks.theme].background)
    yk.floor_shader(tune.floor and FLOOR_SLOT or nil)
    notice_drop(f)
    notice_resize_end(f)
    find_covers(f)
    canvas_cursor(f)

    local dt = math.min(f.dt, 0.05)
    local cam = view.cam
    -- The camera's distance from the plane scales with 1/zoom, so it follows the view as it
    -- stands after this frame's moves.
    local dist = f.cam_dist * f.zoom / cam.zoom

    for id in pairs(seen) do seen[id] = nil end
    for i = 1, f.win_count do
        local id = f.win_id[i]
        seen[id] = true

        local a_to = covering[i] and tune.cover_opacity or 1
        alpha[id] = ease(alpha[id] or 1, a_to, tune.fade_rate, dt)

        z_to[id] = (lifted == id and f.dragging == id) and -tune.lift_height or 0
        z[id] = ease(z[id] or f.win_z[i], z_to[id], tune.lift_rate, dt)

        -- Lowering in place: reproject the release-time centre onto the plane at the current
        -- lift, around the camera, so the window comes straight down on screen.
        local st = settling[id]
        if st then
            local keep = math.abs(z[id]) > 0.5
            local zz = keep and z[id] or 0
            local k = (zz + dist) / (st.z0 + dist)
            local cx = cam.cx + (st.cx - cam.cx) * k
            local cy = cam.cy + (st.cy - cam.cy) * k
            local x, y = cx - f.win_w[i] * 0.5, cy - f.win_h[i] * 0.5
            f.win_x[i], f.win_y[i] = x, y
            yk.move_window(id, x, y)
            if not keep or f.win_max[i] then settling[id] = nil end
        end

        yk.window_look(id, alpha[id], z[id], looks.shadow(f, i), stretched(f, i, id))
        local slot, p1, p2, p3, p4 = looks.window_shader(f, i)
        yk.window_shader(id, slot, p1, p2, p3, p4)
    end

    -- Forget windows that have gone.
    for id in pairs(alpha) do
        if not seen[id] then
            alpha[id], z[id], z_to[id], settling[id], holding[id] = nil, nil, nil, nil, nil
        end
    end
end

return looks
