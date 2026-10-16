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
    -- Behind the canvas, 0xRRGGBBAA.
    background = 0x181820ff,

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

function looks.frame(f)
    yk.set_background(tune.background)
    notice_drop(f)
    find_covers(f)

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

        yk.window_look(id, alpha[id], z[id], looks.shadow(f, i))
    end

    -- Forget windows that have gone.
    for id in pairs(alpha) do
        if not seen[id] then
            alpha[id], z[id], z_to[id], settling[id] = nil, nil, nil, nil
        end
    end
end

return looks
