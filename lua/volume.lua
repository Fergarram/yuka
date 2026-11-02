--
-- Volume
--
-- The volume keys: a tap steps the default output, holding sweeps it, mute toggles it, and
-- the meter shows where it is. The sound server does the work (PipeWire through wpctl);
-- yuka only decides what the keys mean.
--
-- The level lives in the sound server, not here. It is read when a key is pressed after a
-- quiet spell, so a change made elsewhere (a mixer, a headset's buttons) is where the next
-- press starts from. Reading blocks for a few milliseconds, which is why it is not done every
-- press of a burst.
--

local keys = require("keys")
local meter = require("meter")

local volume = {}

--
-- Settings
--

volume.settings = {
    -- The output to drive, as wpctl names it. @DEFAULT_AUDIO_SINK@ follows whatever is the
    -- default, speakers or headphones.
    sink = "@DEFAULT_AUDIO_SINK@",

    -- One tap, as a fraction of full volume. 0.05 is twenty taps end to end; 0.0625 is
    -- sixteen, as on macOS.
    step = 0.0625,

    -- Holding a key sweeps instead of stepping: after hold_delay seconds, hold_speed of the
    -- range per second. 0.6 goes end to end in about a second and a half.
    hold_delay = 0.3,
    hold_speed = 0.6,

    -- While sweeping, the volume is sent at most this often, in seconds, so holding a key
    -- does not start a process every frame. 0.05 still sounds smooth.
    send_every = 0.05,

    -- Seconds without a volume key after which the next press reads the level again.
    reread_after = 2.0,
}

--
-- State
--

-- The volume as last read or set, 0 to 1, and whether the output is muted. nil until a key
-- first reads it.
local level = nil
local muted = false

-- Seconds since a volume key was last pressed or held.
local idle = math.huge

-- How long a volume key has been held, in seconds, 0 when none is.
local held_for = 0

-- Seconds since the level was last sent while sweeping, and whether a change is still unsent.
local since_sent = 0
local unsent = false

--
-- Functions
--

local function run(cmd)
    os.execute("(" .. cmd .. ") >/dev/null 2>&1 &")
end

-- Ask the sound server where the volume is. Leaves level nil when it cannot say.
local function read()
    local pipe = io.popen("wpctl get-volume " .. volume.settings.sink .. " 2>/dev/null")
    if not pipe then return end
    local out = pipe:read("*a") or ""
    pipe:close()
    local value = tonumber(string.match(out, "Volume:%s*([%d%.]+)"))
    if not value then return end
    level = math.max(0, math.min(1, value))
    muted = string.find(out, "MUTED", 1, true) ~= nil
end

local function ensure_read()
    if level == nil or idle > volume.settings.reread_after then read() end
    idle = 0
end

local function send()
    local sink = volume.settings.sink
    run(string.format("wpctl set-mute %s 0; wpctl set-volume %s %.4f", sink, sink, level))
    since_sent = 0
    unsent = false
end

local function show()
    meter.show("volume", muted and 0 or level)
end

-- Move by this many taps: positive louder, negative quieter. A step unmutes, as on macOS.
-- Rounded to a whole step so taps land on the same levels every time.
function volume.step(steps)
    ensure_read()
    if level == nil then return end
    local step = volume.settings.step
    level = math.max(0, math.min(1, math.floor((level + steps * step) / step + 0.5) * step))
    muted = false
    send()
    show()
end

function volume.toggle_mute()
    ensure_read()
    if level == nil then return end
    muted = not muted
    run("wpctl set-mute " .. volume.settings.sink .. " " .. (muted and "1" or "0"))
    show()
end

-- Every frame: a volume key held past hold_delay, with no modifier, sweeps toward its end.
-- The press itself is a step, bound in init.lua.
function volume.held(frame)
    idle = idle + frame.dt
    since_sent = since_sent + frame.dt
    local plain = not (frame.super or frame.ctrl or frame.alt or frame.shift)
    local down_held = plain and yk.key_down(keys.volumedown)
    local up_held = plain and yk.key_down(keys.volumeup)
    if down_held == up_held or level == nil then
        held_for = 0
        if unsent then send() end
        return
    end
    idle = 0
    held_for = held_for + frame.dt
    if held_for < volume.settings.hold_delay then return end
    local move = volume.settings.hold_speed * frame.dt
    level = math.max(0, math.min(1, level + (up_held and move or -move)))
    muted = false
    unsent = true
    if since_sent >= volume.settings.send_every then send() end
    show()
end

return volume
