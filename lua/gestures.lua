--
-- Gestures
--
-- What the recognised gestures mean. The engine (touch.rs) decides that three fingers
-- swiped or tapped; this decides what happens because of it.
--

local view = require("view")
local windows = require("windows")

local gestures = {}

--
-- Tuning
--

gestures.tune = {
    -- Where a three-finger tap at 1:1 steps back to, and the scale of the overview.
    overview_zoom = 0.4,

    -- How far a three-finger tap steps back out of the window you are working in, as a
    -- fraction of the scale you are at. Relative rather than a destination, because stepping
    -- back means "a bit further out than this". It never goes past overview_zoom. 1.0 leaves
    -- the scale alone and the tap only lets go.
    tap_step_out = 0.7,

    -- Which way a swipe reaches. true is the canvas following your fingers: swipe down and
    -- the canvas comes down with your hand, bringing you to whatever was above. false sends
    -- the view the way your fingers went, a joystick rather than a sheet of paper.
    swipe_natural = true,

    -- How far the view slides while the fingers are moving, as a fraction of swipe_nudge_px at
    -- the moment the trigger is reached. Under 1 on purpose, so letting go still has
    -- somewhere to go.
    swipe_preview_frac = 0.7,

    -- How far that slide reaches at full travel, in screen pixels, so the shove looks the same
    -- however far out you are.
    swipe_nudge_px = 140.0,

    -- How far past the trigger the slide can be pulled, as a fraction of the distance to it.
    -- The end of a rubber band rather than a wall: every further millimetre buys less than
    -- the one before, approaching this and never reaching it.
    swipe_stretch_frac = 0.5,

    -- And how hard it fights past the trigger. 1.0 gives at the same rate as before it, 2.0
    -- halves the give the moment you pass it, 5.0 is a firm step into resistance.
    swipe_resist = 4.0,
}

local tune = gestures.tune

--
-- State
--

-- A swipe being made: where the view was when the fingers landed. Everything the gesture does
-- is measured from here, so the slide cannot bias the search for where it goes.
local preview = nil

-- The last left press on a window under Super, and the last middle press, for double clicks.
local last_left_ms, last_left_id = -100000, nil
local last_middle_ms = -100000

--
-- Functions
--

local function swipe_way(frame)
    if tune.swipe_natural then
        return -frame.swipe_dx, -frame.swipe_dy
    end
    return frame.swipe_dx, frame.swipe_dy
end

-- Three fingers. While they move the view slides that way, part of the distance, so the
-- gesture is something you can feel and take back by lifting early. Lifting past the
-- threshold goes to the nearest window that way, or back where you started if there is none.
local function swipe(frame)
    local cam = view.cam
    if frame.swiping then
        if not preview then
            preview = { cx = cam.cx, cy = cam.cy }
        end
        local progress = frame.swipe_progress
        local base = tune.swipe_preview_frac
        local travel
        if progress <= 1 then
            travel = base * progress
        else
            local room = math.max(1 + tune.swipe_stretch_frac - base, 0.0001)
            local give = base / tune.swipe_resist
            travel = base + room * (1 - 1 / (1 + (progress - 1) * give / room))
        end
        local dx, dy = swipe_way(frame)
        local slide = travel * tune.swipe_nudge_px / math.max(cam.zoom, 0.0001)
        view.stop()
        view.set(preview.cx + dx * slide, preview.cy + dy * slide, cam.zoom)
    end

    if frame.swipe_fired then
        local origin_x = preview and preview.cx or cam.cx
        local origin_y = preview and preview.cy or cam.cy
        local dx, dy = swipe_way(frame)
        local i = windows.nearest_in_direction(frame, origin_x, origin_y, dx, dy)
        if i then
            windows.go_to(frame, i, true)
        else
            view.go_to(cam.zoom, origin_x, origin_y, true)
        end
        preview = nil
    end

    -- The fingers left without going far enough: back to where the view was.
    if not frame.swiping and preview then
        view.go_to(cam.zoom, preview.cx, preview.cy, true)
        preview = nil
    end
end

-- Three fingers tapped twice. One gesture with three answers:
--
-- Over a window you are not already in, it goes to that window. Otherwise it is about the
-- view and lets go of every window: from inside a window it steps back a little, at 1:1 it
-- steps out to the overview, and from anywhere else it comes home to 1:1 where you point.
local function tap(frame)
    if not frame.tap3 then return end
    local cam = view.cam
    local x, y = frame.x or 0, frame.y or 0
    local i = frame.under ~= frame.focused and windows.index(frame, frame.under) or nil
    if i then
        windows.go_to(frame, i, false)
        return
    end
    yk.release_all()
    local target
    if frame.focused and tune.tap_step_out < 1 then
        target = math.max(cam.zoom * tune.tap_step_out, tune.overview_zoom)
    elseif math.abs(cam.zoom - cam.default) < view.HOME then
        target = tune.overview_zoom
    else
        target = cam.default
    end
    view.place(nil)
    view.turn_to(target, x, y, false)
end

-- Super with a double click on a window brings it to you. It eats the press, so the second
-- click cannot also pick the window up and move it out from under the view on its way there.
-- Returns whether it did.
local function double_click(frame)
    local eaten = false
    if frame.pressed and frame.super and not frame.dragging and frame.under then
        if frame.time_ms - last_left_ms <= frame.double_click_ms
            and last_left_id == frame.under then
            last_left_ms, last_left_id = -100000, nil
            local i = windows.index(frame, frame.under)
            if i then
                yk.front(frame.under)
                windows.go_to(frame, i, false)
                yk.eat_press()
                eaten = true
            end
        else
            last_left_ms, last_left_id = frame.time_ms, frame.under
        end
    end

    -- Super with a double middle click resets the zoom around the pointer.
    if frame.middle_pressed then
        if frame.super and frame.time_ms - last_middle_ms <= frame.double_click_ms then
            last_middle_ms = -100000
            view.stop()
            view.reset(frame.x, frame.y)
        else
            last_middle_ms = frame.time_ms
        end
    end
    return eaten
end

-- Returns whether the left press was taken, so nothing after this starts a drag from it.
function gestures.frame(frame)
    swipe(frame)
    local eaten = double_click(frame)
    tap(frame)
    return eaten
end

return gestures
