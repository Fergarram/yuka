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
-- Tuning
--

windows.tune = {
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
    startup = { "weston-terminal", "weston-editor", "weston-clickdot", "weston-smoke" },

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
}

local tune = windows.tune

--
-- Lookups
--

-- The snapshot index of a window, or nil when it is not on the canvas.
function windows.index(f, id)
    if not id then return nil end
    for i = 1, f.win_count do
        if f.win_id[i] == id then return i end
    end
    return nil
end

function windows.center(f, i)
    return f.win_x[i] + f.win_w[i] * 0.5, f.win_y[i] + f.win_h[i] * 0.5
end

-- The nearest window in a direction from a canvas point, or nil when there is nothing that way.
--
-- A cone rather than a half plane: a window at right angles to the swipe is not "that way",
-- however close it is. Within the cone the score is how far along the direction it lies plus a
-- penalty for how far off the line, so a window straight ahead beats a nearer one to the side.
local SPREAD = 1.0
local OFF_AXIS = 2.0

function windows.nearest_in_direction(f, ox, oy, dx, dy)
    local best, best_score = nil, nil
    for i = 1, f.win_count do
        local cx, cy = windows.center(f, i)
        local vx, vy = cx - ox, cy - oy
        local along = vx * dx + vy * dy
        -- Behind you, or where you already are.
        if along > 1 then
            local across = math.abs(vx * dy - vy * dx)
            if across <= along * SPREAD then
                local score = along + across * OFF_AXIS
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
    for r = 1, #tune.spawn_order do
        if tune.spawn_order[r] == dir then return r end
    end
    return 99
end

-- Whether a rectangle lands clear of every window but `skip`, with a gap around it.
local function spot_free(f, skip, x, y, w, h, gap)
    for i = 1, f.win_count do
        if i ~= skip then
            local bx, by, bw, bh = f.win_x[i], f.win_y[i], f.win_w[i], f.win_h[i]
            if x - gap < bx + bw and bx < x + w + gap and y - gap < by + bh and by < y + h + gap then
                return false
            end
        end
    end
    return true
end

-- The nearest place to (cx, cy) where a w by h window lands on nothing. Rings outward from the
-- centre, taking the best free candidate on the first ring that has one, so the answer is near
-- by construction. The step is half a window: fine enough not to miss a gap that would have
-- fitted, coarse enough to cross a screenful in a handful of rings.
local RINGS = 16

local function free_spot(f, skip, cx, cy, w, h)
    local step = math.max(math.max(w, h) * 0.5, 64)
    for ring = 0, RINGS do
        local best_rank, best_d2, best_x, best_y = nil, nil, nil, nil
        for dy = -ring, ring do
            for dx = -ring, ring do
                -- The perimeter only: the inside was searched by the rings before.
                if ring == 0 or math.abs(dx) == ring or math.abs(dy) == ring then
                    local ox, oy = cx + dx * step, cy + dy * step
                    local x = math.floor(ox - w * 0.5 + 0.5)
                    local y = math.floor(oy - h * 0.5 + 0.5)
                    if spot_free(f, skip, x, y, w, h, tune.spawn_gap) then
                        local rank = direction_rank(direction_of(dx, dy))
                        local d2 = (ox - cx) ^ 2 + (oy - cy) ^ 2
                        if not best_rank or rank < best_rank or (rank == best_rank and d2 < best_d2) then
                            best_rank, best_d2, best_x, best_y = rank, d2, x, y
                        end
                    end
                end
            end
        end
        if best_x then return best_x, best_y end
    end
    -- Crowded for sixteen rings in every direction: land in the middle and overlap, which is at
    -- least where you are looking.
    return math.floor(cx - w * 0.5 + 0.5), math.floor(cy - h * 0.5 + 0.5)
end

local CASCADE_STEP = 48
local CASCADE_WRAP = 6
local cascade = 0

-- Put a window that just mapped somewhere, before it is first drawn. The snapshot is updated
-- too, so anything later this frame (a journey to it) sees where it went.
local function place(f, i)
    local cam = view.cam
    local w, h = f.win_w[i], f.win_h[i]
    local x, y
    if tune.spawn_clear then
        x, y = free_spot(f, i, cam.cx, cam.cy, w, h)
    else
        local step = (cascade % CASCADE_WRAP) * CASCADE_STEP
        cascade = cascade + 1
        x = math.floor(cam.cx - w * 0.5 + step + 0.5)
        y = math.floor(cam.cy - h * 0.5 + step + 0.5)
    end
    f.win_x[i], f.win_y[i] = x, y
    yk.move_window(f.win_id[i], x, y)
end

--
-- Going to a window
--

-- Give a window the keyboard and send the view to it, at the scale it asks for.
function windows.go_to(f, i, swipe)
    yk.focus(f.win_id[i])
    local zoom = view.fit(f.win_w[i], f.win_h[i], f.win_max[i])
    view.place(zoom)
    local cx, cy = windows.center(f, i)
    view.go_to(zoom, cx, cy, swipe)
end

--
-- Events
--

local function welcome(f, k)
    if tune.focus_new == "never" then return false end
    if tune.focus_new == "same-client" then return f.mapped_welcome[k] end
    return true
end

-- Windows that drew their first frame, and windows another client asked to activate.
function windows.events(f)
    if f.frame == 0 then
        for k = 1, #tune.startup do
            yk.spawn(tune.startup[k])
        end
    end

    for k = 1, f.mapped_count do
        local id = f.mapped[k]
        local i = windows.index(f, id)
        if i then place(f, i) end
        if welcome(f, k) then
            if tune.spawn_travel and i then
                windows.go_to(f, i, false)
            else
                yk.focus(id)
            end
        end
    end

    -- Raised as well as focused, since the point of an activation is that you want to see the
    -- thing. And gone to, unless it is already all on screen: partly visible counts as not
    -- here, and moving the view to something you can already see would be motion for its own
    -- sake.
    for k = 1, f.activated_count do
        local id = f.activated[k]
        yk.front(id)
        local i = windows.index(f, id)
        if i then
            local lx, ty, rx, by = view.rect()
            local x, y, w, h = f.win_x[i], f.win_y[i], f.win_w[i], f.win_h[i]
            if x >= lx and y >= ty and x + w <= rx and y + h <= by then
                yk.focus(id)
            else
                windows.go_to(f, i, false)
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
function windows.maximize_requests(f)
    for k = 1, f.max_count do
        local id = f.max_id[k]
        if f.max_on[k] then
            local lx, ty, rx, by = view.rect()
            yk.maximize(id, lx, ty, rx - lx, by - ty)
        else
            yk.unmaximize(id)
        end
    end
end

-- Click to focus. The window under a press takes the keyboard, and a press on empty canvas
-- lets go. Not the stack: clicking something to type in it should not rearrange the desk, and
-- raising is what picking a window up with Super+drag is for. Not under Super, which is the
-- canvas's, nor for a press something earlier this frame already took.
function windows.click_focus(f, eaten)
    -- Asked every time, not only when it changes: the engine skips a focus that is already
    -- in force, and asks again for one a popup grab refused earlier.
    if f.pressed and not eaten and not f.super then
        yk.focus(f.under)
    end
end

-- A focused window that has left the view lets go of the keyboard. Typing into something you
-- cannot see is the way to put a paragraph somewhere you will not find it.
--
-- Gone means gone entirely: a window one pixel of which is on screen is one you are still
-- working in. And not while the view is travelling, since a journey to a window focuses it as
-- it sets off, when the window is by definition still off screen.
function windows.drop_unseen_focus(f)
    if not f.focused or view.travelling() then return end
    local i = windows.index(f, f.focused)
    if not i then return end
    local lx, ty, rx, by = view.rect()
    local x, y, w, h = f.win_x[i], f.win_y[i], f.win_w[i], f.win_h[i]
    if x + w < lx or x > rx or y + h < ty or y > by then
        yk.focus(nil)
    end
end

return windows
