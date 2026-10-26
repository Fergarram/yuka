--
-- Snapping
--
-- While a window is moved or resized, its edges land on a grid on the canvas. The engine still
-- runs the drag; this says where it lands each frame (yk.drag_to, yk.resize_to), and a spring
-- carries the window from line to line, so it slides into place rather than jumping.
--
-- The spring runs between grid positions, never toward the pointer: the pointer only picks
-- which line is the target. So its small movements, which do not change the nearest line, do
-- not reach the window at all, however soft the spring.
--
-- The grid is its own: it matches the floor's dots by default (looks.lua, floor), but the two
-- are not tied, and either can change or go without the other.
--
-- A lifted window is not snapped: the pointer meets it on its own plane, and these sums are on
-- the canvas.
--

local view = require("view")
local windows = require("windows")

local snap = {}

--
-- Tuning
--

snap.tune = {
    -- Whether moving and resizing a window snaps it to the grid. false and every drag goes
    -- exactly where the pointer puts it.
    enabled = true,

    -- Canvas units between grid lines. Set it to the floor's spacing to land windows on the
    -- dots; 8 is fine grained, 48 coarse.
    grid = 24,

    -- The spring that carries a window from line to line, written like the view's (view.lua):
    -- spring_hz is how fast it wants to move, roughly the round trips a second it would make
    -- with no damping, and spring_damping how much of that oscillation survives:
    --
    --   1.0   critically damped: the fastest arrival with no overshoot at all
    --   0.8   a small overshoot and settle, which reads as weight
    --   0.5   a visible bounce on every line
    --
    -- Around 10 Hz keeps up with a quick hand; 4 drifts behind it.
    spring_hz = 20,
    spring_damping = 1.0,
}

local tune = snap.tune

--
-- State
--

-- The move in progress: which window, the pointer's offset from its top-left when grabbed, and
-- where the window is drawn and the line it is heading for. seen is set once the engine reports
-- the drag, so a move that never started is not waited on forever.
local move = { id = nil, grab_dx = 0, grab_dy = 0, seen = false,
    x = 0, y = 0, vx = 0, vy = 0, target_x = 0, target_y = 0 }

-- A move let go of before its spring came to rest on its line: the rest of the way, keeping the
-- speed it had.
local settle = { id = nil, x = 0, y = 0, vx = 0, vy = 0, target_x = 0, target_y = 0 }

-- The resize in progress: which window, the corner held, where it was grabbed and the window's
-- rectangle then, and the size it is drawn at and how fast that is changing.
local resize = { id = nil, edge_x = 1, edge_y = 1, grab_x = 0, grab_y = 0,
    from_x = 0, from_y = 0, from_w = 0, from_h = 0, w = 0, h = 0, vw = 0, vh = 0,
    seen = false }

--
-- Functions
--

-- How finely the springs are integrated: steps per radian of the spring's motion. A step too
-- long for the stiffness overshoots further each time and flies apart, so the step shrinks as
-- spring_hz grows, and any setting holds. 4 keeps the error well under a pixel.
local STEPS_PER_RADIAN = 4

-- One axis of a spring: where it is and how fast it is going, carried toward target over dt.
local function spring(position, velocity, target, dt)
    if dt <= 0 then return position, velocity end
    local omega = 2 * math.pi * tune.spring_hz
    local damping = 2 * tune.spring_damping * omega
    local steps = math.ceil(dt * math.max(omega, damping) * STEPS_PER_RADIAN)
    local step = dt / steps
    for _ = 1, steps do
        velocity = velocity + (omega * omega * (target - position) - damping * velocity) * step
        position = position + velocity * step
    end
    return position, velocity
end

-- Whether a spring axis has come to rest on its target, not merely passing through it.
local function at_rest(position, velocity, target)
    return math.abs(position - target) < 0.05 and math.abs(velocity) < 1
end

-- The grid line nearest a canvas coordinate.
local function to_grid(value)
    return math.floor(value / tune.grid + 0.5) * tune.grid
end

-- A move picked up, by Super+drag or a titlebar: at screen_x, screen_y, which is the point
-- the engine holds under the pointer.
function snap.begin_move(frame, i, screen_x, screen_y)
    local pointer_x, pointer_y = view.to_canvas(screen_x, screen_y)
    move.id = frame.win_id[i]
    move.grab_dx, move.grab_dy = frame.win_x[i] - pointer_x, frame.win_y[i] - pointer_y
    move.x, move.y = frame.win_x[i], frame.win_y[i]
    move.vx, move.vy = 0, 0
    move.target_x, move.target_y = move.x, move.y
    move.seen = false
    if settle.id == move.id then settle.id = nil end
end

-- A resize picked up holding a corner, or the corner changing hands mid drag. Either way it is
-- measured from here on, as the engine measures it.
function snap.begin_resize(frame, i, edge_x, edge_y)
    local id = frame.win_id[i]
    -- A corner changing hands keeps the size being drawn, so nothing jumps.
    if resize.id ~= id then
        resize.w, resize.h = frame.win_w[i], frame.win_h[i]
        resize.vw, resize.vh = 0, 0
        resize.seen = false
    end
    resize.id = id
    resize.edge_x, resize.edge_y = edge_x, edge_y
    resize.grab_x, resize.grab_y = view.to_canvas(frame.x, frame.y)
    resize.from_x, resize.from_y = frame.win_x[i], frame.win_y[i]
    resize.from_w, resize.from_h = frame.win_w[i], frame.win_h[i]
end

local function step_move(frame, dt)
    if frame.dragging ~= move.id then
        -- Let go, or never started. One let go of still sliding onto its line finishes the trip.
        if move.seen and not (at_rest(move.x, move.vx, move.target_x)
            and at_rest(move.y, move.vy, move.target_y)) then
            settle.id = move.id
            settle.x, settle.y, settle.vx, settle.vy = move.x, move.y, move.vx, move.vy
            settle.target_x, settle.target_y = move.target_x, move.target_y
        end
        if move.seen or frame.dragging then move.id = nil end
        return
    end
    move.seen = true
    local i = windows.index(frame, move.id)
    if not i or not frame.x or frame.win_z[i] ~= 0 then return end

    local pointer_x, pointer_y = view.to_canvas(frame.x, frame.y)
    local raw_x, raw_y = pointer_x + move.grab_dx, pointer_y + move.grab_dy
    if not tune.enabled then
        -- The engine follows the pointer; keep up with it in case snapping comes back on.
        move.x, move.y, move.target_x, move.target_y = raw_x, raw_y, raw_x, raw_y
        move.vx, move.vy = 0, 0
        return
    end
    move.target_x, move.target_y = to_grid(raw_x), to_grid(raw_y)
    move.x, move.vx = spring(move.x, move.vx, move.target_x, dt)
    move.y, move.vy = spring(move.y, move.vy, move.target_y, dt)
    yk.drag_to(move.x, move.y)
    frame.win_x[i], frame.win_y[i] = move.x, move.y
end

local function step_settle(frame, dt)
    local id = settle.id
    local i = windows.index(frame, id)
    if not i or frame.dragging == id then
        settle.id = nil
        return
    end
    settle.x, settle.vx = spring(settle.x, settle.vx, settle.target_x, dt)
    settle.y, settle.vy = spring(settle.y, settle.vy, settle.target_y, dt)
    if at_rest(settle.x, settle.vx, settle.target_x)
        and at_rest(settle.y, settle.vy, settle.target_y) then
        settle.x, settle.y = settle.target_x, settle.target_y
        settle.id = nil
    end
    yk.move_window(id, settle.x, settle.y)
    frame.win_x[i], frame.win_y[i] = settle.x, settle.y
end

local function step_resize(frame, dt)
    if frame.resizing ~= resize.id then
        if resize.seen or frame.resizing then resize.id = nil end
        return
    end
    resize.seen = true
    local i = windows.index(frame, resize.id)
    if not i or not frame.x then return end

    local pointer_x, pointer_y = view.to_canvas(frame.x, frame.y)
    local raw_w = resize.from_w + resize.edge_x * (pointer_x - resize.grab_x)
    local raw_h = resize.from_h + resize.edge_y * (pointer_y - resize.grab_y)
    if not tune.enabled then
        resize.w, resize.h, resize.vw, resize.vh = raw_w, raw_h, 0, 0
        return
    end
    -- The held edges land on grid lines; the opposite ones stay where they were.
    local right, bottom = resize.from_x + resize.from_w, resize.from_y + resize.from_h
    local target_w, target_h
    if resize.edge_x > 0 then
        target_w = to_grid(resize.from_x + raw_w) - resize.from_x
    else
        target_w = right - to_grid(right - raw_w)
    end
    if resize.edge_y > 0 then
        target_h = to_grid(resize.from_y + raw_h) - resize.from_y
    else
        target_h = bottom - to_grid(bottom - raw_h)
    end
    resize.w, resize.vw = spring(resize.w, resize.vw, target_w, dt)
    resize.h, resize.vh = spring(resize.h, resize.vh, target_h, dt)
    yk.resize_to(resize.w, resize.h)
end

function snap.frame(frame)
    local dt = math.min(frame.dt, 0.05)
    if move.id then step_move(frame, dt) end
    if settle.id then step_settle(frame, dt) end
    if resize.id then step_resize(frame, dt) end
end

return snap
