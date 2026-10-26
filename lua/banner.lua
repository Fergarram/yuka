--
-- Error banner
--
-- Why the scripts are not running, across the top of the screen: a reload that would not load,
-- or yk.frame or yk.draw raising an error. The engine calls yk.draw_error with the message every
-- frame there is one, after yk.draw. After a failed reload these are the scripts that were
-- already running, so the banner is never drawn by the file that broke.
--
-- If this raises an error itself, or yk.draw_error is not defined, the engine draws its own
-- banner in its built-in copy of Plex Mono, so a mistake here cannot hide the error.
--
-- Only runs while there is an error, so splitting the message into lines each frame is fine.
--

local text = require("text")

local banner = {}

--
-- Settings
--

banner.settings = {
    -- The bar and the text, 0xRRGGBBAA. Dark red with pale text is hard to miss without
    -- being a flashing alarm.
    bg = 0x3c0c10ff,
    fg = 0xffbeb4ff,

    -- Space around the text and between lines, in pixels.
    pad = 8,
    leading = 4,

    -- How many lines of the message to show. The first is where it went wrong; a traceback past
    -- a few lines belongs in the log.
    max_lines = 6,
}

--
-- Draw
--

function banner.draw(msg, screen_w, screen_h)
    local settings = banner.settings
    local line_h = text.size + settings.leading
    local count = 0
    for _ in msg:gmatch("[^\n]+") do
        count = count + 1
        if count == settings.max_lines then break end
    end
    yk.draw_rect(0, 0, screen_w, count * line_h - settings.leading + settings.pad * 2, settings.bg)
    local drawn = 0
    for line in msg:gmatch("[^\n]+") do
        text.draw(line, settings.pad, settings.pad + drawn * line_h, settings.fg)
        drawn = drawn + 1
        if drawn == settings.max_lines then break end
    end
end

return banner
