--
-- Notch
--
-- A status bar shaped like a notch: a black tab at the top middle of the screen with the date
-- and time in it. Hidden until the pointer touches the top edge near the middle, then it
-- slides down, and stays while the pointer rests on it. Once the pointer leaves it waits a
-- moment and slides back up.
--
-- Drawn over everything, but it does not take input: a click on it reaches whatever is under.
--

local text = require("text")

local notch = {}

--
-- Tuning
--

notch.tune = {
    -- How wide the stretch of the top edge that opens it is, in pixels, centred on the screen.
    -- About the notch's own width feels like touching it; wider is easier to hit by flinging
    -- the pointer up.
    trigger_w = 360,

    -- How close to the top the pointer has to be, in pixels. 1 is the edge itself, which a
    -- pointer flung upward always reaches since the screen holds it there.
    trigger_h = 1,

    -- Seconds to slide fully down, and back up. 0.18 is quick without popping; 0.3 and up
    -- starts to feel like it is waiting for you.
    open_secs = 0.18,
    close_secs = 0.22,

    -- Seconds it stays after the pointer leaves it. Without a pause it snaps shut when the
    -- pointer only grazes past the edge on its way back.
    linger_secs = 0.4,

    -- The tab and the text, 0xRRGGBBAA.
    bg = 0x000000ff,
    fg = 0xffffffff,

    -- os.date formats: the date and the time, drawn side by side with a gap between.
    -- "%a %d %b" is "Sun 25 Oct"; "%H:%M" a 24 hour clock, "%I:%M %p" a 12 hour one.
    date_format = "%a %d %b",
    time_format = "%H:%M",

    -- Space around the text and between the date and time, in pixels.
    pad_x = 22,
    pad_y = 10,
    gap = 16,

    -- How round the bottom corners are, in pixels.
    radius = 14,
}

--
-- State
--

-- How far it is open, 0 hidden to 1 down, and seconds since the pointer was last on it.
local open = 0
local away = math.huge
local screen_w = 0

-- The strings drawn, rebuilt only when the clock ticks over a second, and their widths.
local stamp = -1
local date_s, time_s = "", ""
local date_w, time_w = 0, 0

--
-- Functions
--

-- The tab's size in pixels.
local function size()
    local t = notch.tune
    return date_w + t.gap + time_w + t.pad_x * 2, text.size + t.pad_y * 2
end

local function refresh_clock()
    local now = os.time()
    if now == stamp then return end
    stamp = now
    date_s = os.date(notch.tune.date_format, now)
    time_s = os.date(notch.tune.time_format, now)
    date_w = text.width(date_s)
    time_w = text.width(time_s)
end

-- Whether the pointer is on the trigger strip, or on the tab while it is out.
local function pointed(f)
    if f.x == nil then return false end
    local t = notch.tune
    local mid = f.sw * 0.5
    if f.y < t.trigger_h and math.abs(f.x - mid) <= t.trigger_w * 0.5 then
        return true
    end
    if open == 0 then return false end
    local w, h = size()
    return f.y < h * open and math.abs(f.x - mid) <= w * 0.5
end

--
-- Frame
--

function notch.frame(f)
    local t = notch.tune
    screen_w = f.sw
    refresh_clock()
    if pointed(f) then
        away = 0
    else
        away = away + f.dt
    end
    if away < t.linger_secs then
        open = math.min(1, open + f.dt / t.open_secs)
    else
        open = math.max(0, open - f.dt / t.close_secs)
    end
end

--
-- Draw
--

function notch.draw()
    if open == 0 then return end
    local t = notch.tune
    local w, h = size()
    -- Eased out, so it arrives softly and leaves the same way backwards.
    local e = 1 - (1 - open) ^ 3
    local x = math.floor((screen_w - w) * 0.5)
    local y = math.floor(-h * (1 - e))
    local r = t.radius
    -- Square at the top, where it meets the edge, and round at the bottom.
    yk.draw_rect(x, y, w, h - r, t.bg)
    yk.draw_rect(x + r, y + h - r, w - r * 2, r, t.bg)
    yk.draw_circle(x + r, y + h - r, r, t.bg)
    yk.draw_circle(x + w - r, y + h - r, r, t.bg)
    local ty = y + t.pad_y
    text.draw(date_s, x + t.pad_x, ty, t.fg)
    text.draw(time_s, x + t.pad_x + date_w + t.gap, ty, t.fg)
end

return notch
