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
--   yk.toggle_debug(name)         "labels", "fps" or "pad"
--   yk.close(id)                  ask a window to close
--   yk.screenshot()               save the frame to shot-<n>.png
--   yk.set_camera(cx, cy, zoom)   put the view here (view.lua does this for you)
--   yk.focus(id)                  give a window the keyboard, nil for nobody
--   yk.front(id)                  bring a window to the top of the stack
--   yk.eat_press()                this frame's left press goes no further
--   yk.release_all()              let go of every window
--   yk.begin_move(id, lift)       pick a window up at the pointer, consuming the press
--   yk.begin_resize(id, ex, ey)   resize from the pointer holding a corner, 1 or -1 per axis
--   yk.resize_corner(ex, ey)      change the corner the resize in progress holds
--   yk.spawn(cmd)                 start a program as a client
--   yk.route(wheel, fingers, pinch)
--                                 whether each goes to the client under the pointer
--   yk.key_down(code)             whether a key is held right now
--
-- The frame snapshot (f):
--
--   key_count, key_code[i], key_press[i]   key edges this frame, evdev codes
--   super, ctrl, alt, shift                modifiers held
--   x, y                                   pointer in screen pixels, nil without one
--   dx, dy                                 pointer motion this frame
--   pressed, middle_pressed                left and middle press edges
--   wheel, hwheel, middle                  mouse wheels in notches, middle button held
--   scroll_x, scroll_y                     finger scroll, positive up and right
--   scroll_lifted                          libinput says the scrolling fingers lifted
--   pad_driving, fingers                   our pad code is driving, contacts on the pad
--   pinch                                  pinch factor this frame, 1 for none
--   swiping, swipe_progress                three fingers down, travel toward firing
--   swipe_dx, swipe_dy, swipe_fired        their direction, and the lift that commits
--   tap3                                   three fingers tapped twice
--   focused, hovered, under                window ids, or nil
--   dragging, right_pressed                a window is being moved, the right button went down
--   resizing, resize_ex, resize_ey         our resize in progress and the corner it holds
--   zoom, cx, cy, sw, sh                   the view and the screen
--   dt, time_ms, frame                     the clock
--   zoom_min, zoom_max, zoom_default       the engine's zoom limits
--   double_click_ms                        the double click gap
--   win_count, win_id[i], win_x[i], win_y[i], win_w[i], win_h[i], win_max[i]
--                                          every toplevel, drawn rectangle in canvas units
--   mapped_count, mapped[i], mapped_welcome[i]
--                                          windows that drew their first frame, and whether
--                                          each is from the client with the keyboard
--   activated_count, activated[i]          windows another client asked to activate
--
-- Ctrl+Alt+Backspace (quit) and Ctrl+Alt+Fn (switch VT) are the engine's and cannot
-- be rebound: they are how you get out when a script is wrong.
--

local keys = require("keys")
local view = require("view")
local windows = require("windows")
local gestures = require("gestures")
local grab = require("grab")

yk.config = require("config")

local cfg = yk.config

--
-- Tuning
--

local tune = {
    -- Canvas pixels panned per horizontal wheel notch, at zoom 1.
    hwheel_pan = 60.0,
}

--
-- Bindings
--

-- Chords by modifier set, then by key code. The modifier set is always spelled in the
-- same order, super ctrl alt shift, so a chord matches however it was written.
local binds = {}

local MOD_ORDER = { "super", "ctrl", "alt", "shift" }

local function mods_of(f)
    local m = ""
    if f.super then m = m .. "super+" end
    if f.ctrl then m = m .. "ctrl+" end
    if f.alt then m = m .. "alt+" end
    if f.shift then m = m .. "shift+" end
    return m
end

-- bind("super+w", fn). fn is called with the frame on the press of the last key while
-- exactly those modifiers are held.
local function bind(chord, fn)
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
    local m = ""
    for i = 1, #MOD_ORDER do
        if want[MOD_ORDER[i]] then m = m .. MOD_ORDER[i] .. "+" end
    end
    binds[m] = binds[m] or {}
    binds[m][code] = fn
end

local function dispatch_keys(f)
    if f.key_count == 0 then return end
    local by_code = binds[mods_of(f)]
    if not by_code then return end
    for i = 1, f.key_count do
        if f.key_press[i] then
            local fn = by_code[f.key_code[i]]
            if fn then fn(f) end
        end
    end
end

-- Reload on demand, for when you want to know that it took.
bind("super+r", function() yk.reload() end)

-- Debug overlays: labels under every window, the frame counter, the trackpad instrument.
bind("super+i", function() yk.toggle_debug("labels") end)
bind("super+f", function() yk.toggle_debug("fps") end)
bind("super+p", function() yk.toggle_debug("pad") end)

bind("super+s", function() yk.screenshot() end)

-- Asks the focused window to close, the way every desktop's close chord does.
bind("super+w", function(f) yk.close(f.focused) end)

-- Back to 1:1 around the middle of the screen.
bind("super+0", function()
    view.stop()
    view.reset()
end)

-- Lets go of every window and leaves the view exactly where it is. A key with no
-- direction cannot say where to go, and made to guess it moved the canvas out from
-- under work that only wanted the keyboard back.
bind("super+escape", function() yk.release_all() end)

-- Held keys, read every frame rather than on the press.
--
-- Super with plus or minus zooms around the middle of the screen, since the keyboard has
-- no cursor to anchor to. Exponential, so a step feels the same at any scale.
local function held_keys(f)
    if not f.super then return end
    local zoom_in = yk.key_down(keys.equal) or yk.key_down(keys.kpplus)
    local zoom_out = yk.key_down(keys.minus) or yk.key_down(keys.kpminus)
    if zoom_in == zoom_out then return end
    local step = 1 + view.tune.zoom_rate_per_sec * f.dt
    view.stop()
    view.zoom_at(zoom_in and step or 1 / step, f.sw * 0.5, f.sh * 0.5)
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

local function route(f)
    -- Super takes the wheel and the fingers for the canvas; otherwise they go to the window
    -- being worked in. Nothing focused means nothing is being worked in, so they go to the
    -- canvas, and you do not have to reach for Super to move the view you are looking at.
    --
    -- Focus rather than hover, because hover is where the cursor happens to be resting and
    -- focus is what you last chose. Click a window and two fingers scroll it; click the
    -- canvas and the same two fingers move the canvas.
    local on_client = not f.super and f.focused ~= nil and f.hovered ~= nil

    -- The wheel is not latched: a notch is a whole event with nothing to be halfway through.
    local scrolling = f.scroll_x ~= 0 or f.scroll_y ~= 0
    if scrolling and not scroll.active then
        scroll.active = true
        scroll.to_client = on_client
    end
    -- Over when the fingers that made it are no longer both there, or when libinput says
    -- they lifted. Not on the pad merely saying the scroll stopped leading, which is what a
    -- swipe does as it slows down before letting go, and not on the whole hand leaving
    -- either: a palm left on the pad must not keep a finished gesture alive.
    if f.scroll_lifted or (f.pad_driving and f.fingers < 2) then
        scroll.active = false
    end
    local fingers_to_client = scroll.active and scroll.to_client
    yk.route(on_client, fingers_to_client, on_client)

    -- What the canvas does with what is its own. The wheel and a pinch are zooms of their
    -- own, so whatever the view was travelling toward is not where it is going any more.
    local x, y = f.x or 0, f.y or 0
    if f.super or f.focused == nil then
        if f.wheel ~= 0 then
            local wheel = cfg.invert_scroll == false and f.wheel or -f.wheel
            view.stop()
            view.zoom_at(1.15 ^ wheel, x, y)
        end
        if f.hwheel ~= 0 then
            local hwheel = cfg.invert_scroll == false and f.hwheel or -f.hwheel
            view.pan(hwheel * tune.hwheel_pan, 0)
        end
        if f.pinch ~= 1 then
            view.stop()
            view.zoom_at(f.pinch, x, y)
        end
    end
    -- A gesture that began on the canvas finishes on the canvas, whatever the keyboard has
    -- done since. The view moves against the fingers so the canvas travels with them.
    if scroll.active and not scroll.to_client then
        view.pan(-f.scroll_x, f.scroll_y)
    end
    -- Super with the wheel held down drags the canvas; the grabbed point stays under the
    -- cursor because the view moves by exactly the cursor's delta.
    if f.super and f.middle then
        view.pan(-f.dx, -f.dy)
    end
end

--
-- Frame
--

-- The order matters where two things touch the same state: window events first, so a window
-- that just opened is focused before anything asks; input next; then the view settles, and
-- only once it has does a focused window that left it let go.
function yk.frame(f)
    view.begin(f)
    windows.events(f)
    dispatch_keys(f)
    held_keys(f)
    route(f)
    local eaten = gestures.frame(f)
    grab.frame(f, eaten)
    view.step(f, f.pinch ~= 1, f.swiping)
    windows.drop_unseen_focus(f)
    view.finish()
end
