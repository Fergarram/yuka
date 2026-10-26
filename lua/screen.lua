--
-- Screen
--
-- What the display does to every pixel on its way to the panel: warmer at night, dimmer than
-- the backlight goes, more or less contrast, a different gamma. All of it is one curve per
-- channel, set as the display's gamma ramp (yk.gamma), so it costs nothing to draw and covers
-- everything on screen, the cursor included. Nested, the host owns the ramp and none of this
-- shows.
--
-- The curves are built here from the settings below, and changes ease in rather than snap.
--

local keys = require("keys")

local screen = {}

--
-- Settings
--

screen.settings = {
    -- The two ends of the night range: day at 0, night at 1, and every step between is the
    -- two mixed. Each is:
    --
    --   kelvin     white point. 6500 is the panel as it comes; 4500 is a gentle evening,
    --              3400 is plainly orange, 2700 is candle light. Below 2000 blue is all but gone
    --   dim        a multiplier on everything, under whatever the backlight is doing. 1 is off,
    --              0.7 is a dark room, 0.4 is the screen as a night light. Never below 0.1:
    --              the engine holds the brightest output at a tenth so nothing goes black
    --   contrast   around mid grey. 1 is off, 1.1 is punchier, 0.9 is flatter
    --   gamma      applied before contrast. 1 is off, above 1 deepens the shadows, below lifts them
    day = { kelvin = 6500, dim = 1.0, contrast = 1.0, gamma = 1.0 },
    night = { kelvin = 2300, dim = 0.8, contrast = 1.0, gamma = 1.0 },

    -- Where on the range yuka starts, 0 for day and 1 for full night. Kept until a reload;
    -- screen.step_night moves it while running.
    night_start = 1.0,

    -- How far one key press moves along the range. 0.1 is ten presses from day to night.
    night_step = 0.1,

    -- Holding the key sweeps the range instead of stepping it: after hold_delay seconds, it
    -- moves hold_speed of the range per second until let go or at an end. 0.3 is about where
    -- a tap stops being a tap; a speed of 0.8 goes end to end in a little over a second.
    hold_delay = 0.3,
    hold_speed = 0.8,

    -- How fast a change eases in, as a fraction of the gap per second. 1.5 is a slow sunset,
    -- around a couple of seconds; 20 is near enough a snap.
    rate = 1.5,

    -- How finely the curves are sent: points per channel, resampled by the engine to what the
    -- display takes. 256 is already finer than the eye can tell.
    points = 256,

    -- How often, at most, the curves are sent while a change eases in, in seconds. On Intel
    -- (and likely other drivers) setting the ramp waits for a refresh of its own, so each send
    -- costs a frame: sent every frame, a fade runs at half the frame rate. 0.1 is ten a second,
    -- one late frame per send and too fine a step to see in the colour. 0 sends every frame.
    send_every = 0.1,
}

--
-- State
--

-- Where on the range we are, 0 for day and 1 for night.
local night = screen.settings.night_start

-- How long a range key has been held, in seconds, 0 when none is.
local held_for = 0

-- Seconds since the curves were last sent during a change.
local since_sent = 0

-- The look the range points at right now, refilled each frame.
local want = { kelvin = 6500, dim = 1.0, contrast = 1.0, gamma = 1.0 }

-- Where the look is now, easing toward the target. nil until the first frame, which takes the
-- target outright so a reload does not fade from day.
local now = nil

-- The curves sent, reused every time they are rebuilt.
local red, green, blue = {}, {}, {}

--
-- Functions
--

-- The colour of a black body at this temperature, relative to 6500K so that 6500 is white.
-- Tanner Helland's fit to the blackbody table, good from 1000K to 40000K.
local function white_raw(kelvin)
    -- The fit works in hundreds of kelvin.
    local hecto_kelvin = math.max(1000, math.min(40000, kelvin)) / 100
    local r, g, b
    if hecto_kelvin <= 66 then
        r = 255
        g = 99.4708025861 * math.log(hecto_kelvin) - 161.1195681661
    else
        r = 329.698727446 * (hecto_kelvin - 60) ^ -0.1332047592
        g = 288.1221695283 * (hecto_kelvin - 60) ^ -0.0755148492
    end
    if hecto_kelvin >= 66 then
        b = 255
    elseif hecto_kelvin <= 19 then
        b = 0
    else
        b = 138.5177312231 * math.log(hecto_kelvin - 10) - 305.0447927307
    end
    local function unit(channel) return math.max(0, math.min(255, channel)) / 255 end
    return unit(r), unit(g), unit(b)
end

local ref_r, ref_g, ref_b = white_raw(6500)

local function white(kelvin)
    local r, g, b = white_raw(kelvin)
    return math.min(1, r / ref_r), math.min(1, g / ref_g), math.min(1, b / ref_b)
end

local function build(look)
    -- Untouched, the display gets back the ramp it had before yuka, which may be a calibration.
    if look.kelvin == 6500 and look.dim == 1 and look.contrast == 1 and look.gamma == 1 then
        yk.gamma()
        return
    end
    local white_r, white_g, white_b = white(look.kelvin)
    local points = screen.settings.points
    for i = 1, points do
        local x = (i - 1) / (points - 1)
        local level = x ^ look.gamma
        level = (level - 0.5) * look.contrast + 0.5
        level = math.max(0, math.min(1, level)) * look.dim
        red[i], green[i], blue[i] = level * white_r, level * white_g, level * white_b
    end
    for i = #red, points + 1, -1 do
        red[i], green[i], blue[i] = nil, nil, nil
    end
    yk.gamma(red, green, blue)
end

local FIELDS = { "kelvin", "dim", "contrast", "gamma" }

local function target()
    local day_look, night_look = screen.settings.day, screen.settings.night
    for i = 1, #FIELDS do
        local name = FIELDS[i]
        want[name] = day_look[name] + (night_look[name] - day_look[name]) * night
    end
    return want
end

-- How close is close enough to stop easing, per field.
local DONE = { kelvin = 5, dim = 0.002, contrast = 0.002, gamma = 0.002 }

-- Move along the range by this many steps: positive toward night, negative toward day.
-- Rounded to a whole step so presses land on the same points every time.
function screen.step_night(steps)
    local step = screen.settings.night_step
    local stepped = math.floor((night + steps * step) / step + 0.5) * step
    night = math.max(0, math.min(1, stepped))
end

-- Every frame: Super with a brightness key held past hold_delay sweeps toward that end. The
-- press itself is a step, bound in init.lua.
function screen.held(frame)
    local down_held = frame.super and yk.key_down(keys.brightnessdown)
    local up_held = frame.super and yk.key_down(keys.brightnessup)
    if down_held == up_held then
        held_for = 0
        return
    end
    held_for = held_for + frame.dt
    if held_for < screen.settings.hold_delay then return end
    local move = screen.settings.hold_speed * frame.dt
    night = math.max(0, math.min(1, night + (down_held and move or -move)))
end

-- Where on the range we are, 0 for day and 1 for night.
function screen.night()
    return night
end

-- Every frame, but it only sends curves while something is changing.
function screen.frame(frame)
    local want = target()
    if now == nil then
        now = { kelvin = want.kelvin, dim = want.dim, contrast = want.contrast, gamma = want.gamma }
        build(now)
        return
    end
    local moving = false
    local ease = math.min(1, screen.settings.rate * frame.dt)
    for i = 1, #FIELDS do
        local name = FIELDS[i]
        local gap = want[name] - now[name]
        if gap ~= 0 then
            moving = true
            if math.abs(gap) < DONE[name] then
                now[name] = want[name]
            else
                now[name] = now[name] + gap * ease
            end
        end
    end
    if not moving then
        since_sent = 0
        return
    end
    -- The last step always goes out, so a change ends exactly where it was going.
    local settled = true
    for i = 1, #FIELDS do
        if now[FIELDS[i]] ~= want[FIELDS[i]] then settled = false end
    end
    since_sent = since_sent + frame.dt
    if settled or since_sent >= screen.settings.send_every then
        since_sent = 0
        build(now)
    end
end

return screen
