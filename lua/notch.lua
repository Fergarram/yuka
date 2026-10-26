--
-- Notch
--
-- A status bar shaped like a notch: a black tab at the top middle of the screen with the date,
-- the time and the battery in it. Hidden until the pointer touches the top edge near the middle, then it
-- slides down, and stays while the pointer rests on it. Once the pointer leaves it waits a
-- moment and slides back up.
--
-- Drawn over everything, but it does not take input: a click on it reaches whatever is under.
--
-- The battery is read from sysfs only as the notch starts to open. Reading it asks the
-- laptop's embedded controller, which can take long enough to drop a frame, so it is never
-- polled; it is what the battery said when you looked.
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

    -- The battery's directory under /sys/class/power_supply, tried in order; the first with a
    -- capacity file is used. None found shows no battery.
    batteries = { "BAT0", "BAT1", "BAT2", "CMB0" },

    -- Space around the text and between the items, in pixels.
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

-- The battery as last read, "" for none, and its width.
local battery_s, battery_w = "", 0

--
-- Functions
--

-- The first line of a file, or nil when it cannot be read.
local function read_line(path)
    local file = io.open(path, "r")
    if not file then return nil end
    local line = file:read("*l")
    file:close()
    return line
end

-- The battery's sysfs directory and its name there, found once as the scripts load, nil
-- without one.
local battery_dir, battery_name = nil, nil
for i = 1, #notch.tune.batteries do
    local dir = "/sys/class/power_supply/" .. notch.tune.batteries[i]
    if read_line(dir .. "/capacity") then
        battery_dir, battery_name = dir, notch.tune.batteries[i]
        break
    end
end

-- A number from a sysfs file, nil when it cannot be read.
local function read_number(path)
    local line = read_line(path)
    return line and tonumber(line)
end

-- Hours left at the rate the battery is going, to empty or to full by its status, nil when it
-- says nothing useful. Batteries report either charge in microamp hours and current in
-- microamps, or energy in microwatt hours and power in microwatts; the ratio is hours either
-- way. The averaged current is steadier than the instant one where the battery has it.
local function hours_left(status)
    local dir = battery_dir
    local now = read_number(dir .. "/charge_now")
    local full, rate
    if now then
        full = read_number(dir .. "/charge_full")
        rate = read_number(dir .. "/current_avg") or read_number(dir .. "/current_now")
    else
        now = read_number(dir .. "/energy_now")
        full = read_number(dir .. "/energy_full")
        rate = read_number(dir .. "/power_avg") or read_number(dir .. "/power_now")
    end
    if not now or not rate then return nil end
    -- Some report the rate negative while discharging.
    rate = math.abs(rate)
    if rate == 0 then return nil end
    if status == "Discharging" then return now / rate end
    if status == "Charging" and full then return math.max(0, full - now) / rate end
    return nil
end

local function refresh_battery()
    if not battery_dir then return end
    local capacity = read_line(battery_dir .. "/capacity")
    if not capacity then
        battery_s, battery_w = "", 0
        return
    end
    -- "BAT0 40% - discharging, 1h 23m left", the status as sysfs words it: Charging,
    -- Discharging, Full, Not charging or Unknown.
    local status = read_line(battery_dir .. "/status")
    battery_s = battery_name .. " " .. capacity .. "%"
    if status and status ~= "" then
        battery_s = battery_s .. " - " .. string.lower(status)
    end
    -- ", 1h 23m left" or ", 0h 40m to full".
    local hours = hours_left(status)
    if hours then
        local minutes = math.floor(hours * 60 + 0.5)
        local tail = status == "Charging" and " to full" or " left"
        battery_s = string.format("%s, %dh %02dm%s", battery_s, math.floor(minutes / 60),
            minutes % 60, tail)
    end
    battery_w = text.width(battery_s)
end

-- The tab's size in pixels.
local function size()
    local tune = notch.tune
    local w = date_w + tune.gap + time_w + tune.pad_x * 2
    if battery_s ~= "" then w = w + tune.gap + battery_w end
    return w, text.size + tune.pad_y * 2
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
local function pointed(frame)
    if frame.x == nil then return false end
    local tune = notch.tune
    local mid = frame.sw * 0.5
    if frame.y < tune.trigger_h and math.abs(frame.x - mid) <= tune.trigger_w * 0.5 then
        return true
    end
    if open == 0 then return false end
    local w, h = size()
    return frame.y < h * open and math.abs(frame.x - mid) <= w * 0.5
end

--
-- Frame
--

function notch.frame(frame)
    local tune = notch.tune
    screen_w = frame.sw
    refresh_clock()
    if pointed(frame) then
        if open == 0 then refresh_battery() end
        away = 0
    else
        away = away + frame.dt
    end
    if away < tune.linger_secs then
        open = math.min(1, open + frame.dt / tune.open_secs)
    else
        open = math.max(0, open - frame.dt / tune.close_secs)
    end
end

--
-- Draw
--

function notch.draw()
    if open == 0 then return end
    local tune = notch.tune
    local w, h = size()
    -- Eased out, so it arrives softly and leaves the same way backwards.
    local eased = 1 - (1 - open) ^ 3
    local x = math.floor((screen_w - w) * 0.5)
    local y = math.floor(-h * (1 - eased))
    local radius = tune.radius
    -- Square at the top, where it meets the edge, and round at the bottom.
    yk.draw_rect(x, y, w, h - radius, tune.bg)
    yk.draw_rect(x + radius, y + h - radius, w - radius * 2, radius, tune.bg)
    yk.draw_circle(x + radius, y + h - radius, radius, tune.bg)
    yk.draw_circle(x + w - radius, y + h - radius, radius, tune.bg)
    local text_y = y + tune.pad_y
    text.draw(date_s, x + tune.pad_x, text_y, tune.fg)
    text.draw(time_s, x + tune.pad_x + date_w + tune.gap, text_y, tune.fg)
    if battery_s ~= "" then
        text.draw(battery_s, x + tune.pad_x + date_w + tune.gap + time_w + tune.gap, text_y,
            tune.fg)
    end
end

return notch
