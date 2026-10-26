--
-- Windows
--
-- Which window has the keyboard, where the view goes when one appears or asks for
-- attention, and the lookups the gestures need. Windows are named by the ids in the
-- frame snapshot (win_id and friends); a window that has gone stops appearing there.
--

local view = require("view")

local windows = {}

--
-- Settings
--

windows.settings = {
    -- Which new windows take the keyboard as they appear.
    --
    --   "never"         none of them. Every window waits to be clicked, which is the only
    --                   rule that can never interrupt what you are typing
    --   "same-client"   only from the application you were already using, or when nothing
    --                   had focus. A dialog from the app in front of you is something you
    --                   asked for; a window from one left running in the background is not
    --   "always"        all of them
    focus_new = "always",

    -- Whether the view travels to a window that has just opened, on the same journey a double
    -- click makes. A canvas is larger than the screen, so a window that found clear space may
    -- have found it somewhere you are not looking.
    --
    -- Behind focus_new: the view goes to a window exactly when that window is allowed to take
    -- your typing, so an application opening a window you would not be given cannot take your
    -- view either.
    spawn_travel = true,

    -- Programs started with yuka, so the canvas has something on it.
    startup = { "ghostty", "smerge", "subl", "chromium" },

    -- Whether a new window looks for empty canvas instead of landing on what is already there.
    --
    -- The cascade it replaces is what a desktop does because a desktop has one screenful and
    -- has to stack. A canvas has room, so a window arriving on top of your work is a choice
    -- nobody made. Nearest free space to the middle of the view, so it still arrives where you
    -- are looking. false goes back to the cascade, which steps each new window down and right.
    spawn_clear = true,

    -- The gap it leaves around itself when it does, in canvas units. Enough that two windows
    -- read as two rather than as a seam, small enough that a crowded canvas still finds room.
    spawn_gap = 24.0,

    -- Which directions it tries before others when looking for room, in order. A direction
    -- named here beats one named later, and both beat one not named at all, so { "right" }
    -- means to the right when there is room and whatever is nearest when there is not. Any of
    -- "right", "up", "left", "down"; empty for nearest and nothing else.
    spawn_order = { "right", "down" },

    -- Menus and tooltips are kept inside the view, this many screen pixels in from its edges,
    -- by moving them the way their application allows (flipped to the other side, slid along).
    -- false lets them go wherever they ask, off screen included.
    popup_margin = 8,

    -- Where the keyboard goes when the window that had it closes.
    --
    --   "none"    nowhere: the next window waits to be clicked
    --   "last"    the window you were in before it, if it is still on screen
    --   "under"   whatever is under the pointer
    focus_after_close = "none",

    -- Where a three-finger swipe goes (gestures.lua). First the window the swipe's line runs
    -- into, nearest first, whatever its size or wherever its middle is. Failing that, anything
    -- ahead, nearest first, with a penalty for being off the line, so a crooked swipe still
    -- finds what is clearly there. Only with nothing ahead at all does the view spring back.
    --
    --   swipe_graze_px  how far the line may pass beside a window and still run into it, in
    --                   screen pixels, so grazing an edge counts. 0 is exact, 100 generous
    --   swipe_off_axis  what each unit off the line costs against a unit along it, for windows
    --                   the line misses. 1 weighs them evenly, 3 strongly prefers straight ahead
    swipe_graze_px = 48,
    swipe_off_axis = 2.0,
}

local settings = windows.settings

--
-- Lookups
--

-- The snapshot index of a window, or nil when it is not on the canvas.
function windows.index(frame, id)
    if not id then return nil end
    for i = 1, frame.win_count do
        if frame.win_id[i] == id then return i end
    end
    return nil
end

function windows.center(frame, i)
    return frame.win_x[i] + frame.win_w[i] * 0.5, frame.win_y[i] + frame.win_h[i] * 0.5
end

-- How far along a line from (from_x, from_y) in the direction (dx, dy) it enters a rectangle
-- grown by margin on every side, or nil when it misses it or starts inside it.
local function line_enters(from_x, from_y, dx, dy, x, y, w, h, margin)
    local enter, leave = -math.huge, math.huge
    if math.abs(dx) < 1e-6 then
        if from_x < x - margin or from_x > x + w + margin then return nil end
    else
        local near_t, far_t = (x - margin - from_x) / dx, (x + w + margin - from_x) / dx
        if near_t > far_t then near_t, far_t = far_t, near_t end
        enter, leave = math.max(enter, near_t), math.min(leave, far_t)
    end
    if math.abs(dy) < 1e-6 then
        if from_y < y - margin or from_y > y + h + margin then return nil end
    else
        local near_t, far_t = (y - margin - from_y) / dy, (y + h + margin - from_y) / dy
        if near_t > far_t then near_t, far_t = far_t, near_t end
        enter, leave = math.max(enter, near_t), math.min(leave, far_t)
    end
    if enter > leave or enter <= 0 then return nil end
    return enter
end

-- The window a swipe from a canvas point in the direction (dx, dy), a unit vector, goes to, or
-- nil when there is nothing that way (settings.swipe_graze_px, settings.swipe_off_axis).
function windows.nearest_in_direction(frame, from_x, from_y, dx, dy)
    local margin = settings.swipe_graze_px / math.max(view.cam.zoom, 0.0001)

    -- The window the line runs into first.
    local hit, hit_distance = nil, nil
    for i = 1, frame.win_count do
        if not frame.win_hidden[i] then
            local distance = line_enters(from_x, from_y, dx, dy,
                frame.win_x[i], frame.win_y[i], frame.win_w[i], frame.win_h[i], margin)
            if distance and (not hit_distance or distance < hit_distance) then
                hit, hit_distance = i, distance
            end
        end
    end
    if hit then return hit end

    -- Anything ahead, by its nearest point: along the line, plus a penalty for off it.
    local best, best_score = nil, nil
    for i = 1, frame.win_count do
        if not frame.win_hidden[i] then
            local x, y, w, h = frame.win_x[i], frame.win_y[i], frame.win_w[i], frame.win_h[i]
            local near_x = math.min(math.max(from_x, x), x + w)
            local near_y = math.min(math.max(from_y, y), y + h)
            local offset_x, offset_y = near_x - from_x, near_y - from_y
            local along = offset_x * dx + offset_y * dy
            -- Behind you, or where you already are.
            if along > 1 then
                local across = math.abs(offset_x * dy - offset_y * dx)
                local score = along + across * settings.swipe_off_axis
                if not best_score or score < best_score then
                    best, best_score = i, score
                end
            end
        end
    end
    return best
end

--
-- Placing a new window
--

-- Which of the four directions a ring offset lies in. The dominant axis, so up and slightly
-- right counts as up. A diagonal belongs to no direction and is only taken once the preferred
-- ones are exhausted.
local function direction_of(dx, dy)
    if math.abs(dx) > math.abs(dy) then
        return dx > 0 and "right" or "left"
    elseif math.abs(dy) > math.abs(dx) then
        return dy < 0 and "up" or "down"
    end
    return nil
end

-- How much a direction is preferred, lower being sooner. Unlisted comes after everything listed.
local function direction_rank(dir)
    for rank = 1, #settings.spawn_order do
        if settings.spawn_order[rank] == dir then return rank end
    end
    return 99
end

-- Whether a rectangle lands clear of every window but `skip`, with a gap around it.
local function spot_free(frame, skip, x, y, w, h, gap)
    for i = 1, frame.win_count do
        if i ~= skip then
            local other_x, other_y = frame.win_x[i], frame.win_y[i]
            local other_w, other_h = frame.win_w[i], frame.win_h[i]
            if x - gap < other_x + other_w and other_x < x + w + gap
                and y - gap < other_y + other_h and other_y < y + h + gap then
                return false
            end
        end
    end
    return true
end

-- The nearest place to (center_x, center_y) where a w by h window lands on nothing. Rings
-- outward from the centre, taking the best free candidate on the first ring that has one, so
-- the answer is near by construction. The step is half a window: fine enough not to miss a gap
-- that would have fitted, coarse enough to cross a screenful in a handful of rings.
local RINGS = 16

local function free_spot(frame, skip, center_x, center_y, w, h)
    local step = math.max(math.max(w, h) * 0.5, 64)
    for ring = 0, RINGS do
        local best_rank, best_distance_sq, best_x, best_y = nil, nil, nil, nil
        for dy = -ring, ring do
            for dx = -ring, ring do
                -- The perimeter only: the inside was searched by the rings before.
                if ring == 0 or math.abs(dx) == ring or math.abs(dy) == ring then
                    local candidate_x, candidate_y = center_x + dx * step, center_y + dy * step
                    local x = math.floor(candidate_x - w * 0.5 + 0.5)
                    local y = math.floor(candidate_y - h * 0.5 + 0.5)
                    if spot_free(frame, skip, x, y, w, h, settings.spawn_gap) then
                        local rank = direction_rank(direction_of(dx, dy))
                        local distance_sq = (candidate_x - center_x) ^ 2
                            + (candidate_y - center_y) ^ 2
                        if not best_rank or rank < best_rank
                            or (rank == best_rank and distance_sq < best_distance_sq) then
                            best_rank, best_distance_sq, best_x, best_y = rank, distance_sq, x, y
                        end
                    end
                end
            end
        end
        if best_x then return best_x, best_y end
    end
    -- Crowded for sixteen rings in every direction: land in the middle and overlap, which is at
    -- least where you are looking.
    return math.floor(center_x - w * 0.5 + 0.5), math.floor(center_y - h * 0.5 + 0.5)
end

local CASCADE_STEP = 48
local CASCADE_WRAP = 6
local cascade = 0

-- Put a window that just mapped somewhere, before it is first drawn. The snapshot is updated
-- too, so anything later this frame (a journey to it) sees where it went.
local function place(frame, i)
    local cam = view.cam
    local w, h = frame.win_w[i], frame.win_h[i]
    local x, y
    if settings.spawn_clear then
        x, y = free_spot(frame, i, cam.cx, cam.cy, w, h)
    else
        local step = (cascade % CASCADE_WRAP) * CASCADE_STEP
        cascade = cascade + 1
        x = math.floor(cam.cx - w * 0.5 + step + 0.5)
        y = math.floor(cam.cy - h * 0.5 + step + 0.5)
    end
    frame.win_x[i], frame.win_y[i] = x, y
    yk.move_window(frame.win_id[i], x, y)
end

--
-- Going to a window
--

-- Give a window the keyboard and send the view to it, at the scale it asks for.
function windows.go_to(frame, i, swipe)
    yk.focus(frame.win_id[i])
    local zoom = view.fit(frame.win_w[i], frame.win_h[i], frame.win_max[i])
    view.place(zoom)
    local center_x, center_y = windows.center(frame, i)
    view.go_to(zoom, center_x, center_y, swipe)
end

--
-- Events
--

local function welcome(frame, k)
    if settings.focus_new == "never" then return false end
    if settings.focus_new == "same-client" then return frame.mapped_welcome[k] end
    return true
end

-- The windows that had the keyboard, most recent last, for focus_after_close.
local history = {}

local function remember_focus(frame)
    local id = frame.focused
    if not id or history[#history] == id then return end
    for k = #history, 1, -1 do
        if history[k] == id then table.remove(history, k) end
    end
    history[#history + 1] = id
    if #history > 32 then table.remove(history, 1) end
end

local function on_screen(frame, i)
    local view_left, view_top, view_right, view_bottom = view.rect()
    local x, y, w, h = frame.win_x[i], frame.win_y[i], frame.win_w[i], frame.win_h[i]
    return x + w >= view_left and x <= view_right and y + h >= view_top and y <= view_bottom
end

-- The focused window closed and the engine let go of the keyboard: hand it on, if asked to.
local function focus_after_close(frame)
    if frame.closed_count == 0 or frame.focused then return end
    if settings.focus_after_close == "under" and frame.under then
        yk.focus(frame.under)
    elseif settings.focus_after_close == "last" then
        for k = #history, 1, -1 do
            local i = windows.index(frame, history[k])
            if i and on_screen(frame, i) then
                yk.focus(history[k])
                return
            end
        end
    end
end

-- Windows that drew their first frame, and windows another client asked to activate.
function windows.events(frame)
    remember_focus(frame)
    focus_after_close(frame)
    if frame.frame == 0 then
        for k = 1, #settings.startup do
            yk.spawn(settings.startup[k])
        end
    end

    for k = 1, frame.mapped_count do
        local id = frame.mapped[k]
        local i = windows.index(frame, id)
        if i then place(frame, i) end
        if welcome(frame, k) then
            if settings.spawn_travel and i then
                windows.go_to(frame, i, false)
            else
                yk.focus(id)
            end
        end
    end

    -- Raised as well as focused, since the point of an activation is that you want to see the
    -- thing. And gone to, unless it is already all on screen: partly visible counts as not
    -- here, and moving the view to something you can already see would be motion for its own
    -- sake.
    for k = 1, frame.activated_count do
        local id = frame.activated[k]
        yk.front(id)
        local i = windows.index(frame, id)
        if i then
            local view_left, view_top, view_right, view_bottom = view.rect()
            local x, y, w, h = frame.win_x[i], frame.win_y[i], frame.win_w[i], frame.win_h[i]
            if x >= view_left and y >= view_top
                and x + w <= view_right and y + h <= view_bottom then
                yk.focus(id)
            else
                windows.go_to(frame, i, false)
            end
        else
            yk.focus(id)
        end
    end
end

-- Maximizing on an infinite canvas has to be given a meaning rather than inherited from one.
-- There is no screen for a window to fill: what there is, at any moment, is the part of the
-- canvas you can see. So maximized means exactly that rectangle, taken once when you ask, and
-- fixed in canvas space afterwards: pan away from a maximized window and you leave it behind,
-- the way you leave everything else behind. The zoom is left alone, since a maximized window at
-- 0.6 fills a view you chose to be zoomed out.
-- Fullscreen: the screen's size at 1:1, around the middle of the view, and the view glides to
-- 1:1 so the window covers the screen exactly. Leaving gives the window its rectangle back
-- where the view is.
function windows.fullscreen_requests(frame)
    for k = 1, frame.full_count do
        local id = frame.full_id[k]
        if frame.full_on[k] then
            yk.fullscreen(id, frame.cx - frame.sw * 0.5, frame.cy - frame.sh * 0.5,
                frame.sw, frame.sh)
            view.go_to(1, frame.cx, frame.cy)
        else
            yk.unfullscreen(id)
        end
    end
end

-- The canvas rectangle menus are kept inside: the view, less the margin.
function windows.popup_bounds(frame)
    local margin = settings.popup_margin
    if not margin then
        yk.popup_bounds(nil)
        return
    end
    local w, h = (frame.sw - margin * 2) / frame.zoom, (frame.sh - margin * 2) / frame.zoom
    yk.popup_bounds(frame.cx - w * 0.5, frame.cy - h * 0.5, w, h)
end

function windows.maximize_requests(frame)
    for k = 1, frame.max_count do
        local id = frame.max_id[k]
        if frame.max_on[k] then
            local view_left, view_top, view_right, view_bottom = view.rect()
            yk.maximize(id, view_left, view_top, view_right - view_left, view_bottom - view_top)
        else
            yk.unmaximize(id)
        end
    end
end

-- Click to focus. The window under a press takes the keyboard, and a press on empty canvas
-- lets go. Not the stack: clicking something to type in it should not rearrange the desk, and
-- raising is what picking a window up with Super+drag is for. Not under Super, which is the
-- canvas's, nor for a press something earlier this frame already took.
function windows.click_focus(frame, eaten)
    -- Asked every time, not only when it changes: the engine skips a focus that is already
    -- in force, and asks again for one a popup grab refused earlier.
    if frame.pressed and not eaten and not frame.super then
        yk.focus(frame.under)
    end
end

-- A focused window that has left the view lets go of the keyboard. Typing into something you
-- cannot see is the way to put a paragraph somewhere you will not find it.
--
-- Gone means gone entirely: a window one pixel of which is on screen is one you are still
-- working in. And not while the view is travelling, since a journey to a window focuses it as
-- it sets off, when the window is by definition still off screen.
function windows.drop_unseen_focus(frame)
    if not frame.focused or view.travelling() then return end
    local i = windows.index(frame, frame.focused)
    if not i then return end
    local view_left, view_top, view_right, view_bottom = view.rect()
    local x, y, w, h = frame.win_x[i], frame.win_y[i], frame.win_w[i], frame.win_h[i]
    if x + w < view_left or x > view_right or y + h < view_top or y > view_bottom then
        yk.focus(nil)
    end
end

return windows
