--
-- yuka behaviour
--
-- The engine runs this file, then calls yk.frame once per frame with a snapshot of
-- input and view. Everything here decides; nothing here does. What should happen is
-- asked for through the yk functions, which queue commands the engine applies in
-- order once yk.frame returns.
--
-- Saving any file under lua/ reloads all of them into a fresh VM, so state kept in
-- locals here starts over on a reload. A file that fails to load leaves the previous
-- scripts running and puts the error on screen.
--
-- Engine functions:
--
--   yk.reload()                   re-read the scripts, as a save would
--   yk.close(id)                  ask a window to close
--   yk.screenshot()               save the frame to shot-<n>.png
--   yk.set_camera(cx, cy, zoom)   put the view here (view.lua does this for you)
--   yk.focus(id)                  give a window the keyboard, nil for nobody
--   yk.front(id)                  bring a window to the top of the stack
--   yk.eat_press()                this frame's left press goes no further
--   yk.release_all()              let go of every window
--   yk.begin_move(id)             pick a window up at the pointer, consuming the press
--   yk.accept_move(id, x, y)      answer a titlebar move request, held at that screen point
--   yk.begin_resize(id, ex, ey)   resize from the pointer holding a corner, 1 or -1 per axis
--   yk.resize_corner(ex, ey)      change the corner the resize in progress holds
--   yk.drag_to(x, y)              this frame, put the window being moved here (its visible
--                                 top-left, canvas units) instead of under the pointer
--   yk.resize_to(w, h)            this frame, ask the window being resized for this size
--                                 instead of the pointer's (snap.lua does both for you)
--   yk.spawn(cmd)                 start a program as a client
--   yk.window_look(id, alpha, z, shadow, stretch)
--                                 how a window is drawn: opacity, lift (negative is nearer),
--                                 shadow padding, and whether it is shown at the size a resize
--                                 asks for; kept until changed, children follow
--   yk.shader(slot, vs, fs)       load a window shader into a slot (0 is the built-in one)
--   yk.shader_uniform(slot, name, a [, b, c, d])
--                                 set a uniform on everything a slot draws
--   yk.window_shader(id, slot [, p1, p2, p3, p4])
--                                 which slot draws a window, and its windowParams
--   yk.set_cursor(name)           the cursor over the canvas, by theme name
--   yk.want_inspect(), yk.want_pad()
--                                 gather engine data for yk.surfaces() / yk.pad() this frame
--   yk.set_background(rgba)       the colour behind the canvas, 0xRRGGBBAA
--   yk.client_shadows(on)         whether windows draw their own shadows; false tells them
--                                 they are tiled, which toolkits answer with no shadow
--   yk.client_decorations(on)     whether windows draw their own titlebars; false answers
--                                 both decoration protocols with server side, and yuka draws
--                                 nothing
--   yk.popup_bounds(x, y, w, h)   the canvas rectangle menus and tooltips are kept inside, by
--                                 the rules their application allows; nil for none. Until set,
--                                 the view
--   yk.floor_shader(slot)         which shader slot draws the floor, the canvas plane behind
--                                 every window; nil for none. It is handed frag_uv (the canvas
--                                 point), canvasZoom, time and the slot's uniforms
--   yk.gamma(r [, g, b])          the display's gamma ramp: per channel a list of 2 to 4096
--                                 outputs from 0 to 1, evenly spaced over the inputs (g and b
--                                 default to the one before). The brightest output is held at
--                                 0.1 or more. Kept until changed; yk.gamma() puts back the
--                                 ramp yuka found. Nothing nested (screen.lua does this for you)
--   yk.backlight(fraction)        the panel's backlight, as a fraction of the most it goes to;
--                                 0 is off. Nothing nested or without a backlight
--                                 (backlight.lua does this for you)
--   yk.set_layouts(layout [, variant, options])
--                                 a new keymap from xkb names, comma separated as setxkbmap
--                                 takes them; kept across reloads, only rebuilt on a change
--   yk.set_layout(i)              make the keymap's i-th layout the active one
--   yk.font(path, size)           a font file at a pixel size, as an id for yk.draw_text; the
--                                 same id for the same pair, or nil and why. Ask at load: it is
--                                 built at the next frame (text.lua does this for you)
--
-- Drawing, only inside yk.draw (screen pixels, colours 0xRRGGBBAA):
--
--   yk.draw_rect(x, y, w, h, c)   yk.draw_rect_lines(x, y, w, h, c)
--   yk.draw_line(x1, y1, x2, y2, c)   yk.draw_circle(x, y, r, c)
--   yk.draw_text(text, x, y, size, c [, font])
--                                 in a font from yk.font, raylib's built-in one without
--   yk.measure_text(text, size [, font])
--                                 width and height in pixels; fine outside yk.draw too
--   yk.draw_ellipse_lines(x, y, rx, ry, c)
--   yk.to_screen(x, y, z)         a canvas point at lift z, on screen
--   yk.surfaces()                 every drawable surface, for the labels overlay
--   yk.pad()                      the trackpad as the gesture code sees it, or nil
--   yk.route_scroll(wheel, pinch) whether each goes to the client under the pointer
--   yk.scroll_client(h, v)        a finger scroll for that client this frame, in its own units
--                                 and sign; the engine closes the sequence when it ends
--   yk.route_input(pointer, keys) whether clients get the pointer and key presses
--   yk.move_window(id, x, y)      put a window's visible top-left at a canvas point
--   yk.maximize(id, x, y, w, h)   fill a canvas rectangle with a window
--   yk.unmaximize(id)             give it its own rectangle back
--   yk.fullscreen(id, x, y, w, h), yk.unfullscreen(id)
--                                 the same, telling the client it is fullscreen
--   yk.resize_window(id, w, h)    ask a window to be this size, canvas units; on a window just
--                                 created (frame.created) it is the size it first draws at
--   yk.hide(id [, on])            take a window out of drawing and hit testing, children
--                                 included, or bring it back with on = false
--
-- yk.draw_error(msg, screen_w, screen_h), when defined, draws the error banner every frame
-- the scripts have an error, after yk.draw (banner.lua). One that fails, or none, gets the
-- engine's own.
--
-- Nothing reaches clients unless yk.route_scroll and yk.route_input say so, every frame.
--   yk.key_down(code)             whether a key is held right now
--
-- The frame snapshot (frame):
--
--   key_count, key_code[i], key_press[i]   key edges this frame, evdev codes
--   super, ctrl, alt, shift                modifiers held
--   x, y                                   pointer in screen pixels, nil without one
--   dx, dy                                 pointer motion this frame
--   pressed, middle_pressed                left and middle press edges
--   wheel, hwheel, middle                  mouse wheels in notches, middle button held
--   scroll_x, scroll_y                     finger scroll, positive up and right
--   scroll_lifted                          libinput says the scrolling fingers lifted
--   scroll_ended                           the scroll stopped for any reason
--   pad_driving, fingers                   our pad code is driving, contacts on the pad
--   pinch                                  pinch factor this frame, 1 for none
--   swiping, swipe_progress                three fingers down, travel toward firing
--   swipe_dx, swipe_dy, swipe_fired        their direction, and the lift that commits
--   tap3                                   three fingers tapped twice
--   focused, hovered, under                window ids, or nil
--   dragging, right_pressed                the window being moved, the right button went down
--   resizing, resize_ex, resize_ey         our resize in progress and the corner it holds
--   stretching                             any resize in progress, ours or a client's
--   zoom, cx, cy, sw, sh                   the view and the screen
--   dt, time_ms, frame                     the clock
--   zoom_min, zoom_max, zoom_default       the engine's zoom limits
--   double_click_ms                        the double click gap
--   cam_dist                               camera to canvas distance: lift z shows at
--                                          zoom * cam_dist / (cam_dist + z)
--   win_count, win_id[i], win_x[i], win_y[i], win_w[i], win_h[i], win_max[i], win_order[i],
--   win_z[i], win_rw[i], win_rh[i]         ... lift, and the size a maximized window goes back to
--   win_gw[i], win_gh[i]                   the size the client has committed (win_w is drawn)
--                                          every toplevel, drawn rectangle in canvas units
--   mapped_count, mapped[i], mapped_welcome[i]
--                                          windows that drew their first frame, and whether
--                                          each is from the client with the keyboard
--   created_count, created[i]              windows just created and not drawn yet; size them now
--                                          and they first appear at that size
--   info_count, info_id[i], info_app_id[i], info_title[i], info_parent[i]
--                                          what windows are called and whose dialogs they are
--                                          (parent 0 for none): sent on creation, on change, and
--                                          for every window after a reload. Keep a table by id
--   full_count, full_id[i], full_on[i]     fullscreen requests, answered like maximize ones
--   win_full[i], win_hidden[i]             fullscreen, and hidden by yk.hide
--   activated_count, activated[i]          windows another client asked to activate
--   max_count, max_id[i], max_on[i]        windows asking to be maximized (on) or let go;
--                                          unanswered ones are refused, or given their size back
--   closed_count, closed[i]                windows that closed since the last frame
--   touch_count, touch_x[i], touch_y[i], touch_size[i], touch_resting[i], touch_faint[i]
--                                          every trackpad contact as fractions of the pad
--   move_count, move_id[i], move_x[i], move_y[i]
--                                          titlebar move requests and the press they quote
--   layout, layout_count, layout_names[i]  the active keyboard layout, and every layout's
--                                          full name ("English (US)")
--   backlight, backlight_steps             the panel's backlight as a fraction, and how many
--                                          steps the device has; nil without one yuka can set
--   ime                                    an input method (fcitx5) has the keyboard: a text
--                                          field has focus and it is reading the keys
--
-- Ctrl+Alt+Backspace (quit) and Ctrl+Alt+Fn (switch VT) are the engine's and cannot
-- be rebound: they are how you get out when a script is wrong.
--

local keys = require("keys")
local view = require("view")
local windows = require("windows")
local gestures = require("gestures")
local grab = require("grab")
local snap = require("snap")
local looks = require("looks")
local overlays = require("overlays")
local layouts = require("layouts")
local desktop = require("desktop")
local banner = require("banner")
local screen = require("screen")
local backlight = require("backlight")
local notch = require("notch")

yk.config = require("config")

local cfg = yk.config

--
-- Settings
--

local settings = {
    -- Canvas pixels panned per horizontal wheel notch, at zoom 1.
    hwheel_pan = 60.0,

    -- A multiplier on finger scroll that lands in a window rather than on the canvas. The same
    -- gesture has two destinations: the canvas moves under your fingers one to one, while a
    -- page usually wants to move further than they do. 1.0 sends a window exactly what the
    -- canvas would have panned.
    window_scroll_sens = 0.3,

    -- How much the sideways part of a finger scroll has to be doing, relative to the part
    -- going the way you meant, before a window hears about it at all.
    --
    -- Fingers travelling up a pad wander sideways, and some clients resolve one axis at a time
    -- and end up reading the wander instead of the scroll: ghostty in mouse reporting mode,
    -- which is what tmux puts it in, hears nothing at all when both axes arrive together. So
    -- the axis that leads at the start of a gesture holds it alone, until the other one is
    -- doing real work, at which point both are sent and a map or a wide page scrolls in two
    -- dimensions as it should.
    --
    -- Judged over the last handful of frames rather than the whole gesture, so turning a
    -- corner without lifting your fingers opens it up. 0 always sends both. Larger is
    -- stickier: 1.0 wants the sideways travel to match the other axis before it counts. The
    -- canvas is never locked either way.
    scroll_axis_lock_frac = 0.25,

    -- How much of the recent gesture the axis test remembers: the running travel is multiplied
    -- by this once a frame, a window of roughly ten frames.
    scroll_travel_decay = 0.9,
}

--
-- Bindings
--

-- Chords by modifier set, then by key code. The modifier set is always spelled in the
-- same order, super ctrl alt shift, so a chord matches however it was written.
local binds = {}

local MOD_ORDER = { "super", "ctrl", "alt", "shift" }

local function mods_of(frame)
    local mods = ""
    if frame.super then mods = mods .. "super+" end
    if frame.ctrl then mods = mods .. "ctrl+" end
    if frame.alt then mods = mods .. "alt+" end
    if frame.shift then mods = mods .. "shift+" end
    return mods
end

-- bind("super+w", action). action is called with the frame on the press of the last key
-- while exactly those modifiers are held.
local function bind(chord, action)
    local want = {}
    local key = nil
    for part in string.gmatch(chord, "[^+]+") do
        if part == "super" or part == "ctrl" or part == "alt" or part == "shift" then
            want[part] = true
        else
            key = part
        end
    end
    local code = key and keys[key]
    if not code then
        error("bind: no key in '" .. chord .. "'", 2)
    end
    local mods = ""
    for i = 1, #MOD_ORDER do
        if want[MOD_ORDER[i]] then mods = mods .. MOD_ORDER[i] .. "+" end
    end
    binds[mods] = binds[mods] or {}
    binds[mods][code] = action
end

local function dispatch_keys(frame)
    if frame.key_count == 0 then return end
    local by_code = binds[mods_of(frame)]
    if not by_code then return end
    for i = 1, frame.key_count do
        if frame.key_press[i] then
            local action = by_code[frame.key_code[i]]
            if action then action(frame) end
        end
    end
end

-- Reload on demand, for when you want to know that it took.
bind("super+r", function() yk.reload() end)

-- Debug overlays: labels under every window, the frame counter, the trackpad instrument.
bind("super+i", function() overlays.labels = not overlays.labels end)
bind("super+f", function() overlays.fps = not overlays.fps end)
bind("super+p", function() overlays.pad = not overlays.pad end)

bind("super+s", function() yk.screenshot() end)

-- The backlight, a step per tap of the brightness keys; holding sweeps (backlight.settings).
bind("brightnessdown", function() backlight.step(-1) end)
bind("brightnessup", function() backlight.step(1) end)

-- Night light as a range, like brightness: Super with brightness down steps toward night,
-- with brightness up back toward day, eased in. screen.settings says what each end looks like.
bind("super+brightnessdown", function() screen.step_night(1) end)
bind("super+brightnessup", function() screen.step_night(-1) end)

-- Next keyboard layout, from layouts.settings.list.
bind("super+space", function(frame) layouts.next(frame) end)

-- The JIS Mac keys beside the space bar pick a layout outright, as on macOS: 英数 for Latin,
-- かな for Japanese. Which ones is layouts.settings.eisu and kana.
bind("eisu", function(frame) layouts.select(frame, layouts.settings.eisu) end)
bind("kana", function(frame) layouts.select(frame, layouts.settings.kana) end)

-- Asks the focused window to close, the way every desktop's close chord does.
bind("super+w", function(frame) yk.close(frame.focused) end)

-- A new terminal.
bind("super+shift+t", function() yk.spawn("ghostty") end)

-- Back to 1:1 around the middle of the screen.
bind("super+0", function()
    view.stop()
    view.reset()
end)

-- Lets go of every window and leaves the view exactly where it is. A key with no
-- direction cannot say where to go, and made to guess it moved the canvas out from
-- under work that only wanted the keyboard back.
bind("super+escape", function() yk.release_all() end)

-- Super+Alt with the arrows swipes to the next window that way, like three fingers. It is read
-- from held keys so arrows pressed together make a diagonal (gestures.lua, key_swipe).

-- Held keys, read every frame rather than on the press.
--
-- Super with plus or minus zooms around the middle of the screen, since the keyboard has
-- no cursor to anchor to. Exponential, so a step feels the same at any scale.
local function held_keys(frame)
    screen.held(frame)
    backlight.held(frame)
    if not frame.super then return end
    local zoom_in = yk.key_down(keys.equal) or yk.key_down(keys.kpplus)
    local zoom_out = yk.key_down(keys.minus) or yk.key_down(keys.kpminus)
    if zoom_in == zoom_out then return end
    local step = 1 + view.settings.zoom_rate_per_sec * frame.dt
    view.stop()
    view.zoom_at(zoom_in and step or 1 / step, frame.sw * 0.5, frame.sh * 0.5)
end

--
-- Scroll routing
--

-- A finger scroll's destination, decided on the frame the fingers start moving and held
-- until they lift.
--
-- Held, because otherwise it is re-decided every frame from things the gesture does not
-- control. Let go of Super halfway through panning the canvas and the rest of the gesture
-- would go to whatever the cursor happens to be over: the canvas stops, and an unfocused
-- window's scroll view takes the tail of your pan.
local scroll = { active = false, to_client = false }

-- Which axes a finger scroll toward a client is sending ("none", "v", "h" or "both"), and how far
-- each has travelled lately, as a decaying sum.
local lock = { axes = "none", tx = 0, ty = 0 }

-- Shape this frame's finger scroll for a client: the axis lock, the sensitivity, and the
-- client's sign, which is Wayland's (positive down) flipped by invert_scroll like the wheel.
local function scroll_for_client(frame)
    lock.tx = lock.tx * settings.scroll_travel_decay + math.abs(frame.scroll_x)
    lock.ty = lock.ty * settings.scroll_travel_decay + math.abs(frame.scroll_y)
    -- The first frame decides which axis holds it, a tie going to the vertical.
    if lock.axes == "none" then
        lock.axes = math.abs(frame.scroll_y) >= math.abs(frame.scroll_x) and "v" or "h"
    end
    -- The minor axis takes it back once it is doing real work, and once both are going they
    -- stay going: dropping an axis under a hand still moving is worse than carrying one.
    local frac = settings.scroll_axis_lock_frac
    if lock.axes == "v" and lock.tx >= lock.ty * frac then lock.axes = "both" end
    if lock.axes == "h" and lock.ty >= lock.tx * frac then lock.axes = "both" end
    local sign = cfg.invert_scroll == false and -1 or 1
    local sens = settings.window_scroll_sens
    local vertical = (lock.axes == "v" or lock.axes == "both")
        and frame.scroll_y * sign * sens or 0
    local horizontal = (lock.axes == "h" or lock.axes == "both")
        and frame.scroll_x * -sign * sens or 0
    if vertical ~= 0 or horizontal ~= 0 then
        yk.scroll_client(horizontal, vertical)
    end
end

local function route(frame)
    -- Super takes the wheel and the fingers for the canvas; otherwise they go to the window
    -- being worked in. Nothing focused means nothing is being worked in, so they go to the
    -- canvas, and you do not have to reach for Super to move the view you are looking at.
    --
    -- Focus rather than hover, because hover is where the cursor happens to be resting and
    -- focus is what you last chose. Click a window and two fingers scroll it; click the
    -- canvas and the same two fingers move the canvas.
    local on_client = not frame.super and frame.focused ~= nil and frame.hovered ~= nil

    -- The wheel is not latched: a notch is a whole event with nothing to be halfway through.
    local scrolling = frame.scroll_x ~= 0 or frame.scroll_y ~= 0
    if scrolling and not scroll.active then
        scroll.active = true
        scroll.to_client = on_client
    end
    -- Over when the fingers that made it are no longer both there, or when libinput says
    -- they lifted. Not on the pad merely saying the scroll stopped leading, which is what a
    -- swipe does as it slows down before letting go, and not on the whole hand leaving
    -- either: a palm left on the pad must not keep a finished gesture alive.
    if frame.scroll_lifted or (frame.pad_driving and frame.fingers < 2) then
        scroll.active = false
    end
    local fingers_to_client = scroll.active and scroll.to_client
    yk.route_scroll(on_client, on_client)
    -- Values only for the side that owns the gesture: this frame can be both the last of a
    -- canvas pan and the frame the stop goes out on.
    if fingers_to_client and (frame.scroll_x ~= 0 or frame.scroll_y ~= 0) then
        scroll_for_client(frame)
    end
    if frame.scroll_ended then
        lock.axes, lock.tx, lock.ty = "none", 0, 0
    end

    -- Super is the compositor's modifier. While it is held the canvas claims the pointer, so
    -- zooming and dragging never make the thing under the cursor react, and nothing typed
    -- reaches a client, so Super+S does not also type an s into whatever has focus.
    yk.route_input(not frame.super, not frame.super)

    -- What the canvas does with what is its own. The wheel and a pinch are zooms of their
    -- own, so whatever the view was travelling toward is not where it is going any more.
    local x, y = frame.x or 0, frame.y or 0
    if frame.super or frame.focused == nil then
        if frame.wheel ~= 0 then
            local wheel = cfg.invert_scroll == false and frame.wheel or -frame.wheel
            view.stop()
            view.zoom_at(1.15 ^ wheel, x, y)
        end
        if frame.hwheel ~= 0 then
            local hwheel = cfg.invert_scroll == false and frame.hwheel or -frame.hwheel
            view.pan(hwheel * settings.hwheel_pan, 0)
        end
        if frame.pinch ~= 1 then
            view.stop()
            view.zoom_at(frame.pinch, x, y)
        end
    end
    -- A gesture that began on the canvas finishes on the canvas, whatever the keyboard has
    -- done since. The view moves against the fingers so the canvas travels with them.
    if scroll.active and not scroll.to_client then
        view.pan(-frame.scroll_x, frame.scroll_y)
    end
    -- Super with the wheel held down drags the canvas; the grabbed point stays under the
    -- cursor because the view moves by exactly the cursor's delta.
    if frame.super and frame.middle then
        view.pan(-frame.dx, -frame.dy)
    end
end

--
-- Frame
--

-- The order matters where two things touch the same state: window events first, so a window
-- that just opened is focused before anything asks; input next; then the view settles, and
-- only once it has does a focused window that left it let go. Snapping comes after the view,
-- since it reads the pointer through the camera the frame will be drawn with.
function yk.frame(frame)
    view.begin(frame)
    windows.events(frame)
    dispatch_keys(frame)
    held_keys(frame)
    route(frame)
    local eaten = gestures.frame(frame)
    grab.frame(frame, eaten)
    windows.click_focus(frame, eaten)
    windows.maximize_requests(frame)
    windows.fullscreen_requests(frame)
    windows.popup_bounds(frame)
    view.step(frame, frame.pinch ~= 1, frame.swiping)
    snap.frame(frame)
    windows.drop_unseen_focus(frame)
    looks.frame(frame)
    overlays.frame(frame)
    layouts.frame(frame)
    desktop.frame(frame)
    screen.frame(frame)
    backlight.frame(frame)
    notch.frame(frame)
    view.finish()
end

function yk.draw()
    overlays.draw()
    layouts.draw()
    notch.draw()
end

function yk.draw_error(msg, screen_w, screen_h)
    banner.draw(msg, screen_w, screen_h)
end
