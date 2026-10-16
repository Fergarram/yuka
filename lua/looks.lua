--
-- Looks
--
-- How the canvas looks, apart from the windows' own pixels: the colour behind them, how
-- opaque each one is drawn, and anything drawn over the top. yk.draw runs once a frame
-- after the windows are drawn, in screen pixels; the yk.draw_* functions only work
-- inside it. Colours are 0xRRGGBBAA.
--

local windows = require("windows")

local looks = {}

--
-- Tuning
--

looks.tune = {
    -- Behind the canvas.
    background = 0x181820ff,

    -- How opaque a window is drawn while it covers the one you are working in.
    --
    -- A window in front of the focused one hides work you are looking at. The usual answers
    -- are to raise the focused window or to move something, both of which rearrange the desk
    -- to solve a problem that only lasts while you are looking. Fading the thing on top
    -- instead leaves everything where it is and lets you see through it.
    --
    -- Only windows above the focused one, and only where they actually overlap it. The engine
    -- eases toward this rather than switching, since the answer changes whenever you click,
    -- drag or pan.
    --
    -- 1.0 switches the whole idea off. Below about 0.2 the covering window gets hard to find.
    cover_opacity = 0.9,
}

local tune = looks.tune

--
-- State
--

-- The frame counter, and the last second of frame times it draws from. A run-long maximum
-- is the wrong thing to watch while dragging something: one stall from startup would colour
-- it red forever.
looks.fps = os.getenv("YUKA_DEBUG_FPS") ~= nil
local RING = 60
local ring = {}
for i = 1, RING do ring[i] = 0 end
local slot = 1
local last_dt_ms = 0

--
-- Frame
--

-- Fade whatever is covering the focused window.
local function fade_covers(f)
    local fi = windows.index(f, f.focused)
    if not fi then return end
    local bx, by, bw, bh = f.win_x[fi], f.win_y[fi], f.win_w[fi], f.win_h[fi]
    for i = 1, f.win_count do
        if i ~= fi and f.win_order[i] > f.win_order[fi] then
            local ax, ay, aw, ah = f.win_x[i], f.win_y[i], f.win_w[i], f.win_h[i]
            if ax < bx + bw and bx < ax + aw and ay < by + bh and by < ay + ah then
                yk.window_alpha(f.win_id[i], tune.cover_opacity)
            end
        end
    end
end

function looks.frame(f)
    yk.set_background(tune.background)
    fade_covers(f)

    last_dt_ms = f.dt * 1000
    ring[slot] = last_dt_ms
    slot = slot % RING + 1
end

--
-- Draw
--

-- Frame timing, top left. Three numbers because one is not enough to see lag: the rate, this
-- frame, and the worst frame in the last second. An average hides exactly the stutter you are
-- looking for. Coloured by the worst, against a 60Hz budget: green while every frame fits,
-- amber when one missed, red when one missed badly.
local function draw_fps()
    local sum, worst, n = 0, 0, 0
    for i = 1, RING do
        local v = ring[i]
        if v > 0 then
            sum = sum + v
            if v > worst then worst = v end
            n = n + 1
        end
    end
    local avg = n > 0 and sum / n or 0
    local fps = avg > 0 and 1000 / avg or 0
    local colour = 0xfa785aff
    if worst <= 17 then colour = 0x82e68cff elseif worst <= 25 then colour = 0xf0d26eff end
    local line = string.format("%.0f fps   %.1f ms   worst %.1f ms", fps, last_dt_ms, worst)
    local pad, size = 6, 14
    local w = yk.measure_text(line, size)
    yk.draw_rect(pad, pad, w + pad * 2, size + pad * 2, 0x0c0c10be)
    yk.draw_text(line, pad * 2, pad * 2, size, colour)
end

function looks.draw()
    if looks.fps then draw_fps() end
end

return looks
