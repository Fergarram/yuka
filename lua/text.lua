--
-- Text
--
-- The one face and the one size every piece of text yuka draws is set in: the overlays, the
-- layout pill, anything added later. One size is the design, not a limit of the engine:
-- yk.font builds an atlas per file and size, and another size is one more call.
--
-- The font is built at the first frame after a load (the engine has no GL context while the
-- scripts load), so text drawn before that, or with a file that will not load, falls back to
-- raylib's built-in font.
--

local text = {}

--
-- Tuning
--

text.tune = {
    -- The font file. A monospace face keeps the numbers in the overlays from jittering as
    -- they change. The same directory has the other weights: IBMPlexMono-Text.ttf is a touch
    -- heavier than Regular and holds up better light on dark, -Medium heavier still.
    path = "/usr/share/fonts/truetype/ibm-plex/IBMPlexMono-Regular.ttf",

    -- The size of every line of text, in pixels. 14 reads comfortably at arm's length on a
    -- laptop panel; 12 is dense, 16 and up starts to feel like a heading.
    size = 14,
}

--
-- Font
--

text.size = text.tune.size
local font, err = yk.font(text.tune.path, text.tune.size)
if not font then print("yuka: text: " .. err .. ", using the built-in font") end

--
-- Functions
--

-- Draw a line of text with its top left at x, y, colour 0xRRGGBBAA. Only inside yk.draw.
function text.draw(s, x, y, c)
    yk.draw_text(s, x, y, text.size, c, font)
end

-- Width of a line of text, in pixels.
function text.width(s)
    return (yk.measure_text(s, text.size, font))
end

return text
