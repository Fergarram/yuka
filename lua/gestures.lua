--
-- Gestures
--
-- What the recognised gestures mean. The engine (touch.rs) decides that three fingers
-- swiped or tapped; this decides what happens because of it.
--

local keys = require("keys")
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

    -- While the fingers move, the view slides with them on a rubber band, the way a list
    -- stretches past its end on a phone: one smooth curve from the first millimetre, with no
    -- step at the trigger and no wall. Every further millimetre buys a little less than the one
    -- before, so the pull keeps answering however far your hand goes. Letting go past the
    -- trigger goes; short of it, the view springs back. Both in screen pixels, so it looks the
    -- same at any zoom.
    --
    --   swipe_follow_px  how far the view slides for the travel to the trigger, before the band
    --                    tightens: how directly it follows your fingers at first
    --   swipe_reach_px   the most it can ever slide, which it approaches and never reaches.
    --                    Around 3 times swipe_follow_px is a soft band, 1.5 times a tight one
    swipe_follow_px = 140.0,
    swipe_reach_px = 420.0,

    -- Super+Alt with the arrows is the swipe from the keyboard, the arrows saying which way the
    -- view goes: up and right together is 45 degrees. Arrows pressed within key_chord_ms of the
    -- first count as one chord, since two fingers never land on the same millisecond. 40 needs
    -- a deliberate pair, 120 forgives a sloppy one but answers a single arrow that much later.
    key_chord_ms = 70,
}

local tune = gestures.tune

--
-- State
--

-- A swipe being made: where the view was when the fingers landed. Everything the gesture does
-- is measured from here, so the slide cannot bias the search for where it goes.
local preview = nil

-- A keyboard swipe being gathered: when its first arrow went down (nil when none is), and
-- which arrows have been held since. done once it went, until every arrow is let go.
local chord = { start_ms = nil, left = false, right = false, up = false, down = false,
    done = false }

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
        -- The rubber band: starts at the slope of swipe_follow_px per trigger distance and
        -- flattens toward swipe_reach_px.
        local reach = math.max(tune.swipe_reach_px, 1)
        local pull = frame.swipe_progress * tune.swipe_follow_px
        local slide_px = reach * (1 - 1 / (1 + pull / reach))
        local dx, dy = swipe_way(frame)
        local slide = slide_px / math.max(cam.zoom, 0.0001)
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

-- Super+Alt and the arrows. The arrows held during the chord make the direction, and it goes
-- from where the view is heading rather than where it is, so a second press while the first
-- is still travelling carries on from the window it was going to.
local function key_swipe(frame)
    local held = frame.super and frame.alt
    local left = held and yk.key_down(keys.left)
    local right = held and yk.key_down(keys.right)
    local up = held and yk.key_down(keys.up)
    local down = held and yk.key_down(keys.down)
    if not (left or right or up or down) then
        chord.start_ms, chord.done = nil, false
        chord.left, chord.right, chord.up, chord.down = false, false, false, false
        return
    end
    if chord.done then return end
    chord.start_ms = chord.start_ms or frame.time_ms
    chord.left, chord.right = chord.left or left, chord.right or right
    chord.up, chord.down = chord.up or up, chord.down or down
    if frame.time_ms - chord.start_ms < tune.key_chord_ms then return end

    chord.done = true
    local dx = (chord.right and 1 or 0) - (chord.left and 1 or 0)
    local dy = (chord.down and 1 or 0) - (chord.up and 1 or 0)
    if dx == 0 and dy == 0 then return end
    local length = math.sqrt(dx * dx + dy * dy)
    local origin_x, origin_y = view.heading()
    local i = windows.nearest_in_direction(frame, origin_x, origin_y, dx / length, dy / length)
    if i then windows.go_to(frame, i, true) end
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
    key_swipe(frame)
    local eaten = double_click(frame)
    tap(frame)
    return eaten
end

return gestures
