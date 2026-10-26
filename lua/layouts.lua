--
-- Keyboard layouts
--
-- Which layouts the keymap carries, the chord that steps through them, and a small pill at
-- the bottom of the screen naming the one that just became active, which fades on its own.
-- Also the input method started with yuka, for typing Japanese and the like.
--
-- Layouts are ours and composing is the input method's: fcitx5 turns romaji into kana and
-- kanji on top of whatever layout is active here, and is configured not to manage layouts
-- of its own.
--
-- The pill follows the active layout rather than the chord, so a switch the xkb options
-- make (grp:alt_shift_toggle and the like) shows it too.
--

local text = require("text")

local layouts = {}

--
-- Settings
--

layouts.settings = {
    -- The layouts to step through, in order, by xkb name. A variant goes in brackets the way
    -- setxkbmap prints it: "us(intl)", "de(nodeadkeys)". Empty leaves the keymap to the
    -- XKB_DEFAULT_LAYOUT environment variable, or plain us without one.
    list = { "us", "latam", "jp" },

    -- xkb options, comma separated, as setxkbmap -option takes them: "caps:escape",
    -- "grp:alt_shift_toggle" for a switch xkb makes by itself, "compose:ralt".
    options = "",

    -- What the 英数 and かな keys of a JIS Mac keyboard pick, by their name in list, the way
    -- macOS uses them: one for Latin, one for Japanese. A name not in list does nothing.
    -- fcitx5 can be told to turn composing off and on with the same keys (Hangul_Hanja and
    -- Hangul, as xkb names them), so one press changes both.
    eisu = "us",
    kana = "jp",

    -- The input method, started once when yuka starts and stopped when it exits. A program
    -- name only, no arguments. "" starts none.
    input_method = "fcitx5",

    -- How long the pill stays fully visible after a switch, then how long it takes to fade,
    -- in seconds. 0.8 / 0.3 is a glance; 1.5 / 0.5 is there long enough to read twice.
    hold_secs = 0.8,
    fade_secs = 0.3,

    -- How far the pill's bottom sits above the screen's, in pixels.
    margin = 96,
}

--
-- State
--

-- The active layout last frame, nil until the first frame has been seen, so a reload does
-- not flash the pill. Seconds since the last switch, past hold plus fade once it has gone.
local last = nil
local age = math.huge
local label = ""
local detail = ""
local screen_w, screen_h = 0, 0

--
-- Keymap
--

-- settings.list as the comma separated layout and variant strings xkb takes. Runs once, on
-- load: the engine keeps the keymap across reloads unless these strings change.
local function apply()
    local list = layouts.settings.list
    if #list == 0 then return end
    local names, variants = {}, {}
    for i = 1, #list do
        local name, variant = string.match(list[i], "^([^(]+)%((.-)%)$")
        names[i] = name or list[i]
        variants[i] = variant or ""
    end
    yk.set_layouts(table.concat(names, ","), table.concat(variants, ","), layouts.settings.options)
end

apply()

-- Step to the next layout, wrapping around.
function layouts.next(frame)
    if frame.layout_count < 2 then return end
    yk.set_layout(frame.layout % frame.layout_count + 1)
end

-- Make the layout list names the active one. Already active, or not in list, does nothing.
function layouts.select(frame, name)
    local list = layouts.settings.list
    for i = 1, #list do
        if list[i] == name then
            if i ~= frame.layout and i <= frame.layout_count then yk.set_layout(i) end
            return
        end
    end
end

--
-- Frame
--

function layouts.frame(frame)
    if frame.frame == 0 and layouts.settings.input_method ~= "" then
        yk.spawn(layouts.settings.input_method)
    end
    screen_w, screen_h = frame.sw, frame.sh
    age = age + frame.dt
    if last ~= nil and frame.layout ~= last then
        age = 0
        -- Ours by the name it was asked for, otherwise whatever xkb calls it.
        local ours = layouts.settings.list[frame.layout]
        label = ours and string.upper(ours) or frame.layout_names[frame.layout] or "?"
        detail = frame.layout_names[frame.layout] or ""
        if detail == label then detail = "" end
    end
    last = frame.layout
end

--
-- Draw
--

-- color with its alpha scaled by a, 0 to 1.
local function fade(color, a)
    local alpha = math.floor((color % 256) * a + 0.5)
    return color - color % 256 + alpha
end

function layouts.draw()
    local settings = layouts.settings
    if age >= settings.hold_secs + settings.fade_secs then return end
    local a = 1
    if age > settings.hold_secs then
        a = 1 - (age - settings.hold_secs) / settings.fade_secs
    end
    local pad = text.size
    local w = text.width(label)
    local detail_w = detail ~= "" and text.width(detail) or 0
    local gap = detail ~= "" and pad or 0
    local box_w = w + gap + detail_w + pad * 2
    local box_h = text.size + pad * 2
    local x = math.floor((screen_w - box_w) * 0.5)
    local y = screen_h - settings.margin - box_h
    yk.draw_rect(x, y, box_w, box_h, fade(0x0c0c10d8, a))
    text.draw(label, x + pad, y + pad, fade(0xf0f0f4ff, a))
    if detail ~= "" then
        text.draw(detail, x + pad + w + gap, y + pad, fade(0x9aa0aeff, a))
    end
end

return layouts
