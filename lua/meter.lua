--
-- Meter
--
-- The indicator a level key brings up: a black box at the bottom of the screen with one bar
-- in it, white for how far along the level is and grey for the rest. No label and no icon;
-- brightness, night light and volume all share it, and only the last one changed is shown.
-- It fades on its own once the keys stop.
--
-- A module calls meter.show(source, value) whenever its level moves because of a key. The
-- bar eases toward the new value; a different source, or a meter that had gone, starts the
-- bar where it is told rather than sliding over from someone else's level.
--

local meter = {}

--
-- Settings
--

meter.settings = {
    -- The box, in pixels: its size, corner radius, and the room left and right of the bar.
    -- 256 x 80 with 16 corners and 48 sides is a small tile that reads at a glance.
    box_w = 256,
    box_h = 80,
    box_radius = 16,
    box_pad = 48,

    -- How thick the bar is, in pixels. Its ends are always fully round.
    bar_h = 8,

    -- Colours, 0xRRGGBBAA: the box, the empty part of the bar, the full part.
    box_color = 0x000000ff,
    track_color = 0x3a3a3cff,
    fill_color = 0xffffffff,

    -- How long the meter stays fully visible after the last change, then how long it takes to
    -- fade, in seconds. 1.0 / 0.3 is there while you tap and gone soon after.
    hold_secs = 1.0,
    fade_secs = 0.3,

    -- How fast the bar eases to a new value, as a fraction of the gap per second. 20 follows
    -- a tap without a jump; 60 is near enough a snap.
    rate = 20.0,

    -- How far the box's bottom sits above the screen's, in pixels.
    margin = 96,
}

--
-- State
--

-- Who the meter is showing, where its bar is going and where it is drawn now, 0 to 1.
local source = nil
local target = 0
local shown = 0

-- Seconds since the last change, past hold plus fade once it has gone.
local age = math.huge

local screen_w, screen_h = 0, 0

--
-- Functions
--

local function visible()
    return age < meter.settings.hold_secs + meter.settings.fade_secs
end

-- A level moved because of a key: show it, value from 0 to 1. source is any name that tells
-- this level from the others, "volume" or "backlight".
function meter.show(from, value)
    value = math.max(0, math.min(1, value))
    if from ~= source or not visible() then
        shown = value
    end
    source = from
    target = value
    age = 0
end

function meter.frame(frame)
    screen_w, screen_h = frame.sw, frame.sh
    age = age + frame.dt
    if shown ~= target then
        local gap = target - shown
        if math.abs(gap) < 0.001 then
            shown = target
        else
            shown = shown + gap * math.min(1, meter.settings.rate * frame.dt)
        end
    end
end

--
-- Draw
--

-- color with its alpha scaled by a, 0 to 1.
local function fade(color, a)
    local alpha = math.floor((color % 256) * a + 0.5)
    return color - color % 256 + alpha
end

function meter.draw()
    if not visible() then return end
    local settings = meter.settings
    local a = 1
    if age > settings.hold_secs then
        a = 1 - (age - settings.hold_secs) / settings.fade_secs
    end
    local x = math.floor((screen_w - settings.box_w) * 0.5)
    local y = screen_h - settings.margin - settings.box_h
    yk.draw_rect_rounded(x, y, settings.box_w, settings.box_h, settings.box_radius,
        fade(settings.box_color, a))
    local bar_x = x + settings.box_pad
    local bar_w = settings.box_w - settings.box_pad * 2
    local bar_y = y + math.floor((settings.box_h - settings.bar_h) * 0.5)
    local round = settings.bar_h * 0.5
    yk.draw_rect_rounded(bar_x, bar_y, bar_w, settings.bar_h, round, fade(settings.track_color, a))
    local fill_w = bar_w * shown
    if fill_w > 0.5 then
        yk.draw_rect_rounded(bar_x, bar_y, fill_w, settings.bar_h, round, fade(settings.fill_color, a))
    end
end

return meter
