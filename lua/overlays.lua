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

function overlays.frame(frame)
    last_dt_ms = frame.dt * 1000
    ring[slot] = last_dt_ms
    slot = slot % RING + 1
    zoom = frame.zoom
    screen_w, screen_h = frame.sw, frame.sh
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
    local sum, worst, count = 0, 0, 0
    for i = 1, RING do
        local frame_ms = ring[i]
        if frame_ms > 0 then
            sum = sum + frame_ms
            if frame_ms > worst then worst = frame_ms end
            count = count + 1
        end
    end
    local avg = count > 0 and sum / count or 0
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
    local foreground, background = 0xf0e678ff, 0x000000aa
    local PAD = 3
    local list = yk.surfaces()
    for i = 1, #list do
        local surface = list[i]
        local left_x, bottom_y = yk.to_screen(surface.x, surface.y + surface.h, surface.z)
        local right_x = yk.to_screen(surface.x + surface.w, surface.y + surface.h, surface.z)
        local scale = surface.w > 0 and (right_x - left_x) / surface.w or zoom
        local lines = {
            string.format("%s %s tex%d %dx%d", surface.kind, surface.source, surface.tex_id,
                surface.tex_w, surface.tex_h),
            string.format("%s  scale %.2fx", surface.sampling, scale),
            string.format("canvas %.1f,%.1f  geo %.0f,%.0f %.0fx%.0f  z %.2f",
                surface.canvas_x, surface.canvas_y, surface.geo_x, surface.geo_y,
                surface.geo_w, surface.geo_h, surface.z),
        }
        local widest = 0
        for k = 1, #lines do widest = math.max(widest, text.width(lines[k])) end
        local line_h = text.size + PAD
        local top = bottom_y + PAD
        yk.draw_rect(left_x - PAD, top - PAD, widest + PAD * 2, line_h * #lines + PAD, background)
        for k = 1, #lines do
            text.draw(lines[k], left_x, top + line_h * (k - 1), foreground)
        end
    end
end

--
-- Trackpad
--

-- Bottom right: the surface at its real aspect ratio, the button regions, a dot per finger
-- with its contact ellipse, and what the gesture code currently thinks is happening. An
-- instrument, not content, so screen space.
local function draw_pad(screen_width, screen_height)
    local trackpad = yk.pad()
    if not trackpad then return end
    local WIDTH, MARGIN = 330, 16
    local LINE = text.size + 4
    local background, border, region = 0x0c0c10d2, 0x5a5f6eff, 0x3c4150ff
    local live, hot, dim = 0xf0e678ff, 0xfa785aff, 0x969baaff

    local pad_w = WIDTH
    local pad_h = trackpad.aspect > 0.1 and math.floor(WIDTH / trackpad.aspect) or WIDTH / 2
    local lines = 5
    local panel_h = pad_h + LINE * lines + MARGIN
    local panel_x = screen_width - WIDTH - MARGIN
    local panel_y = screen_height - panel_h - MARGIN
    yk.draw_rect(panel_x - 8, panel_y - 8, WIDTH + 16, panel_h + 16, background)

    local pad_x, pad_y = panel_x, panel_y + LINE * lines
    yk.draw_rect_lines(pad_x, pad_y, pad_w, pad_h, border)

    -- The resting zone: below this line a still finger is treated as parked.
    if trackpad.rest_zone > 0 then
        local rest_y = pad_y + math.floor(pad_h * (1 - trackpad.rest_zone))
        yk.draw_line(pad_x, rest_y, pad_x + pad_w, rest_y, 0x2d323eff)
    end

    -- Button regions: the strip along the bottom, split in two.
    if trackpad.software_buttons then
        local strip_y = pad_y + math.floor(pad_h * (1 - trackpad.strip))
        local split_x = pad_x + math.floor(pad_w * trackpad.split)
        yk.draw_line(pad_x, strip_y, pad_x + pad_w, strip_y, region)
        yk.draw_line(split_x, strip_y, split_x, pad_y + pad_h, region)
        local label_y = pad_y + pad_h - text.size - 4
        text.draw("L", pad_x + 6, label_y, trackpad.left and live or region)
        text.draw("R", split_x + 6, label_y, trackpad.right and live or region)
    end

    -- Fingers. Parked ones are dim, faint ones (too small to count) are in the border colour,
    -- and of the rest the lowest is marked, since it is the one that would decide a click.
    local fingers = trackpad.fingers
    local lowest = nil
    for i = 1, #fingers do
        local finger = fingers[i]
        if not finger.resting and not finger.faint
            and (not lowest or finger.y > fingers[lowest].y) then
            lowest = i
        end
    end
    for i = 1, #fingers do
        local finger = fingers[i]
        local finger_x = pad_x + math.floor(finger.x * pad_w)
        local finger_y = pad_y + math.floor(finger.y * pad_h)
        local colour = live
        if finger.faint then
            colour = border
        elseif finger.resting then
            colour = dim
        elseif trackpad.software_buttons and lowest == i then
            colour = hot
        end
        -- The contact ellipse: how much of the finger is touching. This hardware reports no
        -- per-finger pressure, so the footprint is the only force-like signal there is.
        if trackpad.has_size then
            local radius_y = math.max(finger.major * 0.5 * pad_h, 2.5)
            local radius_x = math.max(finger.minor * 0.5 * pad_h, 2.0)
            yk.draw_ellipse_lines(finger_x, finger_y, radius_x, radius_y, colour)
        end
        yk.draw_circle(finger_x, finger_y, 1.5, colour)
    end

    local function armed(is_armed) return is_armed and "armed" or "idle" end
    local sizes = ""
    for i = 1, math.min(#fingers, 3) do sizes = sizes .. " " .. fingers[i].raw_major end
    if #fingers == 0 then sizes = " -" end
    local lines = {
        string.format("trackpad  %d finger%s  %d counted  max %d",
            trackpad.count, trackpad.count == 1 and "" or "s", trackpad.active,
            trackpad.contact_max),
        string.format("gesture %s  pan %s  zoom %s", trackpad.mode, armed(trackpad.pan_armed),
            armed(trackpad.zoom_armed)),
        string.format("pan %+.1f,%+.1f  zoom %.3f  cursor %s", trackpad.pan_x, trackpad.pan_y,
            trackpad.zoom, armed(trackpad.ptr_armed)),
        string.format("size%s  load %d/%d", sizes, trackpad.load, trackpad.load_max),
        string.format("buttons %s%s%s", trackpad.software_buttons and "regions" or "physical",
            trackpad.left and "  LEFT" or "", trackpad.right and "  RIGHT" or ""),
    }
    for i = 1, #lines do
        local colour = (i == 5 and (trackpad.left or trackpad.right)) and live or dim
        text.draw(lines[i], panel_x, panel_y + LINE * (i - 1), colour)
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
