--
-- Backlight
--
-- The panel's light, from the brightness keys: a tap steps it, holding sweeps it, and it eases
-- toward where it is going either way. Levels here are how bright it looks, 0 to 1, and a
-- curve turns them into the device's own level (yk.backlight), because a backlight's steps
-- are even in light, not in how bright it looks: the bottom few steps change the screen far
-- more than the top few.
--
-- The level lives in the device, not here, so a reload picks up where it was, and a change
-- made elsewhere (the firmware, brightnessctl) is followed once nothing here is moving it.
-- Nested, or on a machine without a backlight yuka can set, f.backlight is nil and none of
-- this runs.
--

local keys = require("keys")

local backlight = {}

--
-- Tuning
--

backlight.tune = {
    -- How a level here maps to the device: device = level ^ curve. 1 is the device's own
    -- steps, even in light, which crowd all the change at the dark end. 2 spreads it out so
    -- every step looks about the same; 2.5 gives the dark end finer still.
    curve = 2.0,

    -- One tap of a brightness key, in levels. 0.05 is twenty taps end to end, like most
    -- desktops.
    step = 0.05,

    -- The dimmest the keys go, in levels. 0 is the light off, a black screen, as on macOS.
    -- With curve 2, 0.1 is 1% of the light, a dark room but still readable. If the scripts
    -- break while it is off, or yuka quits, the engine turns it back on.
    min = 0.0,

    -- Holding a key sweeps instead of stepping: after hold_delay seconds, hold_speed of the
    -- range per second. 0.6 goes end to end in about a second and a half.
    hold_delay = 0.3,
    hold_speed = 0.6,

    -- How fast the light eases to where it is going, as a fraction of the gap per second. 12
    -- feels immediate but not a jump; 30 is near enough a snap.
    rate = 12.0,
}

--
-- State
--

-- Where the light is going, and where it is now, in levels. nil until the first frame with a
-- backlight, which reads both from the device.
local level = nil
local shown = nil

-- The device fraction last sent, to tell our own changes from someone else's.
local sent = nil

-- How long a brightness key has been held, in seconds, 0 when none is.
local held_for = 0

--
-- Functions
--

local function to_level(fraction)
    return fraction ^ (1 / backlight.tune.curve)
end

local function clamp(v)
    return math.max(backlight.tune.min, math.min(1, v))
end

-- Move by this many taps: positive brighter, negative dimmer. Rounded to a whole step so taps
-- land on the same levels every time.
function backlight.step(steps)
    if level == nil then return end
    local step = backlight.tune.step
    level = clamp(math.floor((level + steps * step) / step + 0.5) * step)
end

-- Every frame: a brightness key held past hold_delay, with no modifier, sweeps toward its end.
-- The press itself is a step, bound in init.lua.
function backlight.held(f)
    if level == nil then return end
    local plain = not (f.super or f.ctrl or f.alt or f.shift)
    local down = plain and yk.key_down(keys.brightnessdown)
    local up = plain and yk.key_down(keys.brightnessup)
    if down == up then
        held_for = 0
        return
    end
    held_for = held_for + f.dt
    if held_for < backlight.tune.hold_delay then return end
    local move = backlight.tune.hold_speed * f.dt
    level = clamp(level + (up and move or -move))
end

function backlight.frame(f)
    if f.backlight == nil then return end
    if level == nil then
        level = to_level(f.backlight)
        shown = level
        sent = f.backlight
        return
    end
    -- Settled, and the device is somewhere we did not put it: someone else moved it, so
    -- follow rather than fight.
    local one_step = 1 / f.backlight_steps
    if shown == level and held_for == 0 and math.abs(f.backlight - sent) > one_step then
        level = to_level(f.backlight)
        shown = level
        sent = f.backlight
        return
    end
    if shown == level then return end
    local gap = level - shown
    if math.abs(gap) < 0.002 then
        shown = level
    else
        shown = shown + gap * math.min(1, backlight.tune.rate * f.dt)
    end
    sent = shown ^ backlight.tune.curve
    yk.backlight(sent)
end

return backlight
