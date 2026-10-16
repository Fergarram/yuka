--
-- The view
--
-- Where the camera is and how it gets anywhere. Every frame starts from the engine's
-- camera (view.begin), everything that moves it this frame works on a copy here, and
-- view.finish hands the result back with one yk.set_camera. So a pan, a zoom and a
-- journey in the same frame compose in the order they ran, with nothing in between
-- reading a stale camera.
--

local view = {}

--
-- Tuning
--

view.tune = {
    -- The spring that carries the view when something sends it somewhere: being sent to a
    -- window, the 1:1 detent, a three-finger tap. A pinch is never followed by a journey,
    -- and neither is Super+Escape.
    --
    -- Written as a frequency and a damping ratio rather than a stiffness and a friction,
    -- because those two are the ones you can feel. spring_hz is how fast it wants to move,
    -- roughly the round trips a second it would make with no damping at all. spring_damping
    -- is how much of that oscillation survives:
    --
    --   1.0   critically damped: the fastest arrival with no overshoot at all
    --   0.8   a small overshoot and settle, which reads as weight
    --   0.5   a visible bounce
    --   1.5   heavy and slow, arriving from one side
    --
    -- The two together decide how long a journey takes; there is no separate duration. One
    -- spring drives both the scale and the pan, so they cannot fall out of step, and when it
    -- overshoots they overshoot together.
    spring_hz = 3.5,
    spring_damping = 0.75,

    -- And the same pair for the three-finger swipes, which want a different character.
    --
    -- A swipe already had its motion: your fingers made it, and the spring only finishes what
    -- they started or takes it back, so bounce there reads as the view arguing with your hand.
    -- Being sent to a window is the opposite, a motion the view makes on its own, and a little
    -- overshoot is what gives that weight.
    swipe_spring_hz = 2.5,
    swipe_spring_damping = 0.8,

    -- How close to the default zoom the view may sit and still be carried the rest of the way,
    -- as a difference in scale. 0.05 means anything between 0.95 and 1.05 eases to 1.0, around
    -- wherever the pointer is, so nothing slides.
    --
    -- A zoom of 0.98, or of 1.0004, costs every window on screen a resample and shows nothing
    -- for it: the pixel grid disengages and text softens at a scale nothing was aiming for.
    -- Checked every frame, whatever moved the zoom, because a pinch ending is only one of the
    -- ways to arrive beside 1:1. Fingers actively pinching are the exception: the band is
    -- theirs to move through while they are asking.
    --
    -- A detent, not a mode. It only pulls toward the default and only from inside the band.
    -- 0 switches it off.
    zoom_detent = 0.05,

    -- Breathing room around a window the view is fitted to, in screen pixels per side. Only
    -- for a window too big for the screen, where the fit is ours rather than the window's. A
    -- maximized window is fitted exactly whatever this says, because it was given the shape
    -- of the screen and one with a gap around it is just a large window. 0 fits exactly.
    fit_padding_px = 0.0,

    -- Keyboard zoom rate, for Super with plus or minus.
    zoom_rate_per_sec = 2.0,
}

local tune = view.tune

--
-- State
--

-- This frame's camera, and whether anything moved it.
local cam = { cx = 0, cy = 0, zoom = 1, sw = 1, sh = 1, min = 0.1, max = 8, default = 1 }
local dirty = false
view.cam = cam

-- A journey the view is on, or nil.
--
-- The spring drives a progress from 0 to 1 rather than the zoom and the centre themselves,
-- and both are read off that progress. One spring cannot fall out of step with itself, so
-- the scale and the pan arrive together, and when it overshoots they overshoot together.
--
-- With a destination (to_x, to_y) the centre travels there. Without one the journey turns
-- around a fixed screen point (ax, ay): the scale changes and the view stays where it was
-- pointed.
local journey = nil

-- A scale the view was deliberately sent to, when it is not the default: the fit of a window
-- it was told to go to. Remembered so the detent leaves it alone, since fitting a maximized
-- window lands near 1:1 by construction and the detent would otherwise pull it the rest of
-- the way. Only while the zoom still sits where it was put.
local placed = nil

-- Close enough to 1:1 to count as being there.
view.HOME = 0.01

--
-- Functions
--

local function clamp(z)
    if z < cam.min then return cam.min end
    if z > cam.max then return cam.max end
    return z
end

function view.begin(f)
    cam.cx, cam.cy, cam.zoom = f.cx, f.cy, f.zoom
    cam.sw, cam.sh = f.sw, f.sh
    cam.min, cam.max, cam.default = f.zoom_min, f.zoom_max, f.zoom_default
    dirty = false
end

function view.finish()
    if dirty then
        yk.set_camera(cam.cx, cam.cy, cam.zoom)
    end
end

function view.set(cx, cy, zoom)
    cam.cx, cam.cy, cam.zoom = cx, cy, clamp(zoom)
    dirty = true
end

-- Scale by a factor, keeping the canvas point under the screen point (sx, sy) fixed there.
function view.zoom_at(factor, sx, sy)
    local ox, oy = sx - cam.sw * 0.5, sy - cam.sh * 0.5
    local px, py = cam.cx + ox / cam.zoom, cam.cy + oy / cam.zoom
    cam.zoom = clamp(cam.zoom * factor)
    cam.cx, cam.cy = px - ox / cam.zoom, py - oy / cam.zoom
    dirty = true
end

-- Move the view by screen pixels. Positive x shows more of what is to the right.
function view.pan(dx, dy)
    cam.cx = cam.cx + dx / cam.zoom
    cam.cy = cam.cy + dy / cam.zoom
    dirty = true
end

-- Back to the default zoom, around a screen point or the middle of the screen.
function view.reset(sx, sy)
    view.zoom_at(cam.default / cam.zoom, sx or cam.sw * 0.5, sy or cam.sh * 0.5)
end

-- The canvas point under a screen point.
function view.to_canvas(sx, sy)
    return cam.cx + (sx - cam.sw * 0.5) / cam.zoom, cam.cy + (sy - cam.sh * 0.5) / cam.zoom
end

-- The visible canvas rectangle: left, top, right, bottom.
function view.rect()
    local hw, hh = cam.sw * 0.5 / cam.zoom, cam.sh * 0.5 / cam.zoom
    return cam.cx - hw, cam.cy - hh, cam.cx + hw, cam.cy + hh
end

function view.stop()
    journey = nil
end

function view.travelling()
    return journey ~= nil
end

-- Set off for a canvas point, arriving at a scale. swipe picks the swipe spring.
function view.go_to(zoom, tx, ty, swipe)
    journey = {
        from_zoom = cam.zoom, target = zoom,
        to_x = tx, to_y = ty,
        -- How far the centre was from its destination, in screen pixels. Screen rather than
        -- canvas, because what you watch is the canvas distance times the zoom and the zoom
        -- is moving: eased in canvas units the view barely moves at first and then slides.
        off_x = (tx - cam.cx) * cam.zoom, off_y = (ty - cam.cy) * cam.zoom,
        p = 0, v = 0, swipe = swipe,
    }
end

-- Change scale around a screen point, staying where the view is pointed.
function view.turn_to(zoom, ax, ay, swipe)
    journey = {
        from_zoom = cam.zoom, target = zoom,
        ax = ax, ay = ay,
        p = 0, v = 0, swipe = swipe,
    }
end

-- Remember a scale the view was sent to on purpose, or forget it.
function view.place(zoom)
    if zoom and zoom ~= cam.default then placed = zoom else placed = nil end
end

-- The scale that shows a window the way it asks to be shown.
--
-- 1:1 for an ordinary window that fits, which is its own pixel size. A maximized window took
-- the shape of the view when it was asked for, so the view fits to it exactly. A window too
-- big for the screen gets the view pulled out far enough to hold it, with the padding. Only
-- out, never in: a small window is left at its own size.
function view.fit(w, h, maximized)
    if w <= 0 or h <= 0 then return cam.default end
    local zoom
    if maximized then
        zoom = math.min(cam.sw / w, cam.sh / h)
    else
        local pw = math.max(cam.sw - tune.fit_padding_px * 2, 1)
        local ph = math.max(cam.sh - tune.fit_padding_px * 2, 1)
        zoom = math.min(cam.default, math.min(pw / w, ph / h))
    end
    return clamp(zoom)
end

-- The detent, then the spring. Called once a frame after everything that might start or stop
-- a journey. pinching and swiping are the fingers owning the zoom, which keeps the detent out.
function view.step(f, pinching, swiping)
    local sits_where_placed = placed and math.abs(cam.zoom - placed) < 0.001
    if tune.zoom_detent > 0 and not journey and not pinching and not swiping
        and not sits_where_placed then
        local off = cam.zoom - cam.default
        if off ~= 0 and math.abs(off) <= tune.zoom_detent then
            view.turn_to(cam.default, f.x or 0, f.y or 0, false)
        end
    end

    local j = journey
    if not j then return end

    -- dt is capped, because a frame that took a long time is a stall rather than a slow moment,
    -- and integrating one whole would fling the spring somewhere absurd.
    local dt = math.min(f.dt, 0.05)
    local hz = j.swipe and tune.swipe_spring_hz or tune.spring_hz
    local damping = j.swipe and tune.swipe_spring_damping or tune.spring_damping
    local w = 2 * math.pi * hz
    j.v = j.v + (w * w * (1 - j.p) - 2 * damping * w * j.v) * dt
    j.p = j.p + j.v * dt

    -- At rest at its destination, not merely passing through it: an underdamped spring
    -- crosses the target several times on the way to stopping.
    local landed = math.abs(j.p - 1) < 0.001 and math.abs(j.v) < 0.01
    -- Geometric, so halfway is the geometric mean: scale is not perceived linearly.
    local want = j.target
    if j.from_zoom > 0 and j.target > 0 then
        want = clamp(j.from_zoom * (j.target / j.from_zoom) ^ j.p)
    end

    if j.to_x then
        -- The screen offset it started with shrinks by the same progress, and the centre is
        -- whatever puts that offset on screen at this scale, so the window closes on the middle
        -- of the view at a steady rate rather than sliding into place after the zoom.
        local zoom = clamp(want)
        local left = 1 - j.p
        view.set(j.to_x - j.off_x * left / zoom, j.to_y - j.off_y * left / zoom, zoom)
        if landed then
            view.set(j.to_x, j.to_y, j.target)
            journey = nil
        end
    else
        view.zoom_at(want / cam.zoom, j.ax, j.ay)
        if landed then
            view.zoom_at(j.target / cam.zoom, j.ax, j.ay)
            journey = nil
        end
    end
end

return view
