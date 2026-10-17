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

local screen = {}

--
-- Tuning
--

screen.tune = {
    -- The two looks screen.toggle_night switches between. Each is:
    --
    --   kelvin     white point. 6500 is the panel as it comes; 4500 is a gentle evening,
    --              3400 is plainly orange, 2700 is candle light. Below 2000 blue is all but gone
    --   dim        a multiplier on everything, under whatever the backlight is doing. 1 is off,
    --              0.7 is a dark room, 0.4 is the screen as a night light. Never below 0.1:
    --              the engine holds the brightest output at a tenth so nothing goes black
    --   contrast   around mid grey. 1 is off, 1.1 is punchier, 0.9 is flatter
    --   gamma      applied before contrast. 1 is off, above 1 deepens the shadows, below lifts them
    day = { kelvin = 6500, dim = 1.0, contrast = 1.0, gamma = 1.0 },
    night = { kelvin = 3400, dim = 0.8, contrast = 1.0, gamma = 1.0 },

    -- Start at night. Kept until a reload; screen.toggle_night flips it while running.
    night_on = false,

    -- How fast a change eases in, as a fraction of the gap per second. 1.5 is a slow sunset,
    -- around a couple of seconds; 20 is near enough a snap.
    rate = 1.5,

    -- How finely the curves are sent: points per channel, resampled by the engine to what the
    -- display takes. 256 is already finer than the eye can tell.
    points = 256,
}

--
-- State
--

local night = screen.tune.night_on

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
    local t = math.max(1000, math.min(40000, kelvin)) / 100
    local r, g, b
    if t <= 66 then
        r = 255
        g = 99.4708025861 * math.log(t) - 161.1195681661
    else
        r = 329.698727446 * (t - 60) ^ -0.1332047592
        g = 288.1221695283 * (t - 60) ^ -0.0755148492
    end
    if t >= 66 then
        b = 255
    elseif t <= 19 then
        b = 0
    else
        b = 138.5177312231 * math.log(t - 10) - 305.0447927307
    end
    local function c(v) return math.max(0, math.min(255, v)) / 255 end
    return c(r), c(g), c(b)
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
    local wr, wg, wb = white(look.kelvin)
    local n = screen.tune.points
    for i = 1, n do
        local x = (i - 1) / (n - 1)
        local v = x ^ look.gamma
        v = (v - 0.5) * look.contrast + 0.5
        v = math.max(0, math.min(1, v)) * look.dim
        red[i], green[i], blue[i] = v * wr, v * wg, v * wb
    end
    for i = #red, n + 1, -1 do
        red[i], green[i], blue[i] = nil, nil, nil
    end
    yk.gamma(red, green, blue)
end

local function target()
    return night and screen.tune.night or screen.tune.day
end

local FIELDS = { "kelvin", "dim", "contrast", "gamma" }

-- How close is close enough to stop easing, per field.
local DONE = { kelvin = 5, dim = 0.002, contrast = 0.002, gamma = 0.002 }

function screen.toggle_night()
    night = not night
end

function screen.night()
    return night
end

-- Every frame, but it only sends curves while something is changing.
function screen.frame(f)
    local want = target()
    if now == nil then
        now = { kelvin = want.kelvin, dim = want.dim, contrast = want.contrast, gamma = want.gamma }
        build(now)
        return
    end
    local moving = false
    local k = math.min(1, screen.tune.rate * f.dt)
    for i = 1, #FIELDS do
        local name = FIELDS[i]
        local gap = want[name] - now[name]
        if gap ~= 0 then
            moving = true
            if math.abs(gap) < DONE[name] then
                now[name] = want[name]
            else
                now[name] = now[name] + gap * k
            end
        end
    end
    if moving then
        build(now)
    end
end

return screen
