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

        -- In the overview, picking a window up lifts it toward the camera, which is the canvas
        -- idiom for having it in your hand. At 1:1 it only comes to the front: a lift there is
        -- perspective applied to something already filling the screen, so it swims rather than
        -- rises, and it takes the window off the pixel grid while you are close enough to see.
        if i and f.pressed and not eaten and not f.dragging then
            yk.begin_move(f.under, f.zoom < f.zoom_default)
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
