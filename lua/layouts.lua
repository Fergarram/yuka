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

local layouts = {}

--
-- Tuning
--

layouts.tune = {
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

    -- Text size of the pill, and how far its bottom sits above the screen's, in pixels.
    text_size = 24,
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

-- The tune list as the comma separated layout and variant strings xkb takes. Runs once, on
-- load: the engine keeps the keymap across reloads unless these strings change.
local function apply()
    local list = layouts.tune.list
    if #list == 0 then return end
    local names, variants = {}, {}
    for i = 1, #list do
        local name, variant = string.match(list[i], "^([^(]+)%((.-)%)$")
        names[i] = name or list[i]
        variants[i] = variant or ""
    end
    yk.set_layouts(table.concat(names, ","), table.concat(variants, ","), layouts.tune.options)
end

apply()

-- Step to the next layout, wrapping around.
function layouts.next(f)
    if f.layout_count < 2 then return end
    yk.set_layout(f.layout % f.layout_count + 1)
end

-- Make the layout list names the active one. Already active, or not in list, does nothing.
function layouts.select(f, name)
    local list = layouts.tune.list
    for i = 1, #list do
        if list[i] == name then
            if i ~= f.layout and i <= f.layout_count then yk.set_layout(i) end
            return
        end
    end
end

--
-- Frame
--

function layouts.frame(f)
    if f.frame == 0 and layouts.tune.input_method ~= "" then
        yk.spawn(layouts.tune.input_method)
    end
    screen_w, screen_h = f.sw, f.sh
    age = age + f.dt
    if last ~= nil and f.layout ~= last then
        age = 0
        -- Ours by the name it was asked for, otherwise whatever xkb calls it.
        local ours = layouts.tune.list[f.layout]
        label = ours and string.upper(ours) or f.layout_names[f.layout] or "?"
        detail = f.layout_names[f.layout] or ""
        if detail == label then detail = "" end
    end
    last = f.layout
end

--
-- Draw
--

-- c with its alpha scaled by a, 0 to 1.
local function fade(c, a)
    local alpha = math.floor((c % 256) * a + 0.5)
    return c - c % 256 + alpha
end

function layouts.draw()
    local t = layouts.tune
    if age >= t.hold_secs + t.fade_secs then return end
    local a = 1
    if age > t.hold_secs then
        a = 1 - (age - t.hold_secs) / t.fade_secs
    end
    local size = t.text_size
    local small = math.floor(size * 0.6)
    local pad = math.floor(size * 0.6)
    local w = yk.measure_text(label, size)
    local dw = detail ~= "" and yk.measure_text(detail, small) or 0
    local gap = detail ~= "" and pad or 0
    local box_w = w + gap + dw + pad * 2
    local box_h = size + pad * 2
    local x = math.floor((screen_w - box_w) * 0.5)
    local y = screen_h - t.margin - box_h
    yk.draw_rect(x, y, box_w, box_h, fade(0x0c0c10d8, a))
    yk.draw_text(label, x + pad, y + pad, size, fade(0xf0f0f4ff, a))
    if detail ~= "" then
        yk.draw_text(detail, x + pad + w + gap, y + pad + size - small, small, fade(0x9aa0aeff, a))
    end
end

return layouts
