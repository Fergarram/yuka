--
-- Overlays
--
-- The debug instruments, drawn over the canvas: the frame counter (Super+F), a label under
-- every window saying what it is made of and how it is sampled (Super+I), and the trackpad
-- as the gesture code sees it (Super+P). Each also starts on with its environment variable,
-- YUKA_DEBUG_FPS, YUKA_DEBUG_LABELS or YUKA_DEBUG_PAD.
--
-- The labels and the pad read engine state that is only gathered on frames that ask for it
-- (yk.want_inspect, yk.want_pad), so an overlay that is off costs nothing.
--

local text = require("text")

local overlays = {}

overlays.fps = os.getenv("YUKA_DEBUG_FPS") ~= nil
overlays.labels = os.getenv("YUKA_DEBUG_LABELS") ~= nil
overlays.pad = os.getenv("YUKA_DEBUG_PAD") ~= nil

--
-- State
--

-- The last second of frame times. A run-long maximum is the wrong thing to watch while
-- dragging something: one stall from startup would colour it red forever.
local RING = 60
local ring = {}
for i = 1, RING do ring[i] = 0 end
local slot = 1
local last_dt_ms = 0
local zoom = 1
local screen_w, screen_h = 0, 0

--
-- Frame
--

function overlays.frame(f)
    last_dt_ms = f.dt * 1000
    ring[slot] = last_dt_ms
    slot = slot % RING + 1
    zoom = f.zoom
    screen_w, screen_h = f.sw, f.sh
    if overlays.labels then yk.want_inspect() end
    if overlays.pad then yk.want_pad() end
end

--
-- Frame counter
--

-- Top left. Three numbers because one is not enough to see lag: the rate, this frame, and the
-- worst frame in the last second. An average hides exactly the stutter you are looking for.
-- Coloured by the worst, against a 60Hz budget: green while every frame fits, amber when one
-- missed, red when one missed badly.
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
    local pad = 6
    local w = text.width(line)
    yk.draw_rect(pad, pad, w + pad * 2, text.size + pad * 2, 0x0c0c10be)
    text.draw(line, pad * 2, pad * 2, colour)
end

--
-- Window labels
--

-- Under every surface, children included: what it is, where its pixels come from, how the
-- sampler is set, and the scale it is really drawn at. The scale is measured off the quad
-- through the camera, so a lifted window reports what perspective adds.
local function draw_labels()
    local fg, bg = 0xf0e678ff, 0x000000aa
    local PAD = 3
    local list = yk.surfaces()
    for n = 1, #list do
        local s = list[n]
        local ax, ay = yk.to_screen(s.x, s.y + s.h, s.z)
        local rx = yk.to_screen(s.x + s.w, s.y + s.h, s.z)
        local scale = s.w > 0 and (rx - ax) / s.w or zoom
        local lines = {
            string.format("%s %s tex%d %dx%d", s.kind, s.source, s.tex_id, s.tex_w, s.tex_h),
            string.format("%s  scale %.2fx", s.sampling, scale),
            string.format("canvas %.1f,%.1f  geo %.0f,%.0f %.0fx%.0f  z %.2f",
                s.canvas_x, s.canvas_y, s.geo_x, s.geo_y, s.geo_w, s.geo_h, s.z),
        }
        local widest = 0
        for k = 1, #lines do widest = math.max(widest, text.width(lines[k])) end
        local line_h = text.size + PAD
        local top = ay + PAD
        yk.draw_rect(ax - PAD, top - PAD, widest + PAD * 2, line_h * #lines + PAD, bg)
        for k = 1, #lines do
            text.draw(lines[k], ax, top + line_h * (k - 1), fg)
        end
    end
end

--
-- Trackpad
--

-- Bottom right: the surface at its real aspect ratio, the button regions, a dot per finger
-- with its contact ellipse, and what the gesture code currently thinks is happening. An
-- instrument, not content, so screen space.
local function draw_pad(sw, sh)
    local p = yk.pad()
    if not p then return end
    local WIDTH, MARGIN = 330, 16
    local LINE = text.size + 4
    local bg, frame, region = 0x0c0c10d2, 0x5a5f6eff, 0x3c4150ff
    local live, hot, dim = 0xf0e678ff, 0xfa785aff, 0x969baaff

    local pad_w = WIDTH
    local pad_h = p.aspect > 0.1 and math.floor(WIDTH / p.aspect) or WIDTH / 2
    local lines = 5
    local panel_h = pad_h + LINE * lines + MARGIN
    local x0 = sw - WIDTH - MARGIN
    local y0 = sh - panel_h - MARGIN
    yk.draw_rect(x0 - 8, y0 - 8, WIDTH + 16, panel_h + 16, bg)

    local px, py = x0, y0 + LINE * lines
    yk.draw_rect_lines(px, py, pad_w, pad_h, frame)

    -- The resting zone: below this line a still finger is treated as parked.
    if p.rest_zone > 0 then
        local rest_y = py + math.floor(pad_h * (1 - p.rest_zone))
        yk.draw_line(px, rest_y, px + pad_w, rest_y, 0x2d323eff)
    end

    -- Button regions: the strip along the bottom, split in two.
    if p.software_buttons then
        local strip_y = py + math.floor(pad_h * (1 - p.strip))
        local split_x = px + math.floor(pad_w * p.split)
        yk.draw_line(px, strip_y, px + pad_w, strip_y, region)
        yk.draw_line(split_x, strip_y, split_x, py + pad_h, region)
        local label_y = py + pad_h - text.size - 4
        text.draw("L", px + 6, label_y, p.left and live or region)
        text.draw("R", split_x + 6, label_y, p.right and live or region)
    end

    -- Fingers. Parked ones are dim, faint ones (too small to count) are in the frame colour,
    -- and of the rest the lowest is marked, since it is the one that would decide a click.
    local fingers = p.fingers
    local lowest = nil
    for i = 1, #fingers do
        local c = fingers[i]
        if not c.resting and not c.faint and (not lowest or c.y > fingers[lowest].y) then
            lowest = i
        end
    end
    for i = 1, #fingers do
        local c = fingers[i]
        local cx, cy = px + math.floor(c.x * pad_w), py + math.floor(c.y * pad_h)
        local colour = live
        if c.faint then
            colour = frame
        elseif c.resting then
            colour = dim
        elseif p.software_buttons and lowest == i then
            colour = hot
        end
        -- The contact ellipse: how much of the finger is touching. This hardware reports no
        -- per-finger pressure, so the footprint is the only force-like signal there is.
        if p.has_size then
            local ry = math.max(c.major * 0.5 * pad_h, 2.5)
            local rx = math.max(c.minor * 0.5 * pad_h, 2.0)
            yk.draw_ellipse_lines(cx, cy, rx, ry, colour)
        end
        yk.draw_circle(cx, cy, 1.5, colour)
    end

    local function armed(on) return on and "armed" or "idle" end
    local sizes = ""
    for i = 1, math.min(#fingers, 3) do sizes = sizes .. " " .. fingers[i].raw_major end
    if #fingers == 0 then sizes = " -" end
    local lines = {
        string.format("trackpad  %d finger%s  %d counted  max %d",
            p.count, p.count == 1 and "" or "s", p.active, p.contact_max),
        string.format("gesture %s  pan %s  zoom %s", p.mode, armed(p.pan_armed), armed(p.zoom_armed)),
        string.format("pan %+.1f,%+.1f  zoom %.3f  cursor %s", p.pan_x, p.pan_y, p.zoom, armed(p.ptr_armed)),
        string.format("size%s  load %d/%d", sizes, p.load, p.load_max),
        string.format("buttons %s%s%s", p.software_buttons and "regions" or "physical",
            p.left and "  LEFT" or "", p.right and "  RIGHT" or ""),
    }
    for n = 1, #lines do
        local colour = (n == 5 and (p.left or p.right)) and live or dim
        text.draw(lines[n], x0, y0 + LINE * (n - 1), colour)
    end
end

--
-- Draw
--

function overlays.draw()
    if overlays.labels then draw_labels() end
    if overlays.pad then draw_pad(screen_w, screen_h) end
    if overlays.fps then draw_fps() end
end

return overlays
