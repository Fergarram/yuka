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
        if welcome(f, k) then
            local id = f.mapped[k]
            local i = windows.index(f, id)
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
