--
-- Picking windows up
--
-- Super+drag moves the window under the pointer and Super+right-drag resizes it. This
-- decides when a grab starts, which window, whether it lifts, and which corner a resize
-- holds. The engine does the rest: following the pointer, asking the client for sizes at
-- the rate it can answer, and setting the window down again.
--
-- Behind Super, always: that is the compositor's way of moving and resizing a window, as
-- opposed to the client's own titlebar and edges, which the engine honours on its own.
--

local view = require("view")
local windows = require("windows")
local looks = require("looks")

local grab = {}

--
-- Tuning
--

grab.tune = {
    -- A resize drag can change corners without letting go: whichever side of the window the
    -- cursor is on is the side that moves. Splitting the window down the middle makes that
    -- too easy to trigger, because shrinking inward walks the cursor toward the middle and
    -- crossing it hands the drag over just as you are getting somewhere.
    --
    -- So the corner you grabbed keeps this much of each axis for as long as that drag lasts,
    -- and the boundary moves out of its way. Start in the bottom right and the split sits a
    -- quarter of the way in, leaving that corner three quarters of the window on both axes.
    --
    -- 0.5 is an even split and no bias. Values near 1.0 make it very hard to change corners.
    resize_corner_hold = 0.75,
}

local tune = grab.tune

--
-- Functions
--

-- Picking up a window that is filling the view takes it out of maximized first.
--
-- Only when it is actually filling it. Maximized here is a size, not a mode that owns a screen,
-- and a window keeps it while you pan, zoom or move it, so one still marked maximized may sit in
-- the corner of the view at a third of the size, and dragging that one is just dragging it.
--
-- It comes back under your hand rather than where it used to be: the remembered size is kept,
-- with the grabbed point held at the same place in the window it was grabbed at. Grab the
-- middle of a full-view window and you are still holding the middle of the small one.
local function unmaximize_under(f, i, sx, sy)
    if not f.win_max[i] then return end
    local x, y, w, h = f.win_x[i], f.win_y[i], f.win_w[i], f.win_h[i]
    local lx, ty, rx, by = view.rect()
    if x > lx or y > ty or x + w < rx or y + h < by then return end
    local rw, rh = f.win_rw[i], f.win_rh[i]
    local gx, gy = view.to_canvas(sx, sy)
    local fx = w > 0 and (gx - x) / w or 0.5
    local fy = h > 0 and (gy - y) / h or 0.5
    local nx, ny = gx - fx * rw, gy - fy * rh
    yk.unmaximize(f.win_id[i])
    yk.move_window(f.win_id[i], nx, ny)
    f.win_x[i], f.win_y[i], f.win_w[i], f.win_h[i], f.win_max[i] = nx, ny, rw, rh, false
end

-- Which corner of a window a canvas point pulls on, per axis: 1 for the far edge (right,
-- bottom), -1 for the near one. split_x and split_y are where each axis divides, as a fraction
-- of the window. Measured from the window's edges rather than tested against its bounds, so it
-- still answers once the cursor has left the window, which it does for most of a drag.
local function corner_at(px, py, x, y, w, h, split_x, split_y)
    local ex = px < x + w * split_x and -1 or 1
    local ey = py < y + h * split_y and -1 or 1
    return ex, ey
end

-- Where an axis divides for a drag holding a given side of it. The side being held keeps
-- `hold` of the axis, so the boundary sits out of its way. Asked with the corner in force
-- rather than the one the drag started on, which makes it hysteresis: between the two
-- boundaries the corner does not change at all.
local function corner_split(edge, hold)
    hold = math.min(math.max(hold, 0.5), 0.95)
    return edge > 0 and 1 - hold or hold
end

-- eaten is a press something earlier this frame already took, such as a double click.
function grab.frame(f, eaten)
    if f.super and f.under and f.x then
        local i = windows.index(f, f.under)
        local px, py = view.to_canvas(f.x, f.y)

        -- The corner from the quadrant the press landed in: grab near the top left and the
        -- top left is what moves, the way every other desktop does it. Always a corner, never
        -- a single edge, so there is no middle region where a drag resizes nothing.
        if i and f.right_pressed and not f.resizing then
            local ex, ey = corner_at(px, py, f.win_x[i], f.win_y[i], f.win_w[i], f.win_h[i], 0.5, 0.5)
            yk.begin_resize(f.under, ex, ey)
        end

        -- In the overview, picking a window up lifts it toward the camera (looks.lua), which is the canvas
        -- idiom for having it in your hand. At 1:1 it only comes to the front: a lift there is
        -- perspective applied to something already filling the screen, so it swims rather than
        -- rises, and it takes the window off the pixel grid while you are close enough to see.
        if i and f.pressed and not eaten and not f.dragging then
            unmaximize_under(f, i, f.x, f.y)
            yk.begin_move(f.under)
            if f.zoom < f.zoom_default then looks.pick_up(f.under) end
        end
    end

    -- A client asking to be moved by its titlebar. Neither lifted nor raised: the client asked
    -- to be moved, not picked up, and a drag from inside a window is the same kind of act as a
    -- click in one. It says where the window goes, not what is in front of what.
    for k = 1, f.move_count do
        local id = f.move_id[k]
        local i = windows.index(f, id)
        if i and not f.dragging then
            unmaximize_under(f, i, f.move_x[k], f.move_y[k])
            yk.accept_move(id, f.move_x[k], f.move_y[k])
        end
    end

    -- The corner a resize holds follows your hand: cross the boundary and the opposite corner
    -- takes over. Going back the way you came undoes it, so it reads as pushing the window's
    -- edges around rather than as a mode.
    if f.resizing and f.x then
        local i = windows.index(f, f.resizing)
        if i then
            local px, py = view.to_canvas(f.x, f.y)
            local ex, ey = corner_at(px, py, f.win_x[i], f.win_y[i], f.win_w[i], f.win_h[i],
                corner_split(f.resize_ex, tune.resize_corner_hold),
                corner_split(f.resize_ey, tune.resize_corner_hold))
            if ex ~= f.resize_ex or ey ~= f.resize_ey then
                yk.resize_corner(ex, ey)
            end
        end
    end
end

return grab
