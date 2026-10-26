--
-- yuka settings
--
-- The feel of the thing: sensitivities, gesture thresholds, camera rates. Loaded
-- by init.lua as yk.config and read by the engine after every load, so saving
-- any file under lua/ lands the change on the next frame. Super+R reloads on
-- demand.
--
-- Plain values only: numbers, booleans and strings. Unknown keys are reported
-- and ignored, and a value that will not parse keeps its default. Defaults are
-- what the binary ships with (src/settings.rs); a key removed here falls back to
-- that rather than to zero.
--

return {
    -- Which code drives the trackpad: custom (our raw evdev gestures, touch.rs) or
    -- libinput. Applied when the device is opened, so this one needs a restart.
    trackpad_mode = "custom",

    --
    -- Trackpad: pointer
    --

    -- Cursor pixels per device unit of finger travel.
    pointer_sens = 0.135,
    -- How far a finger must travel, as a fraction of the pad, before it moves the
    -- cursor at all. This is what stops a click dragging the pointer off target.
    pointer_start_frac = 0.005,
    -- Pointer acceleration, as the two ends of a ramp.
    --
    -- One sensitivity has to serve two jobs that want opposite things: crossing the screen,
    -- where the finger should cover ground, and landing on something small, where it should
    -- not. Tuned for the first the second overshoots, tuned for the second your hand runs out
    -- of pad.
    --
    -- So the gain follows how fast the finger is moving. pointer_slow_factor is what
    -- pointer_sens is multiplied by when the finger is barely moving, and
    -- pointer_full_speed_frac is the speed at which it reaches full sensitivity, measured in
    -- pad widths per second. In between it ramps linearly.
    --
    -- This only ever slows the cursor down, never past pointer_sens, so a long fast movement
    -- feels exactly as it did. 1.0 for the factor turns it off entirely.
    --
    --   0.5 / 1.0   half speed for fine work, full speed at a brisk flick
    --   0.3 / 1.5   much more precision, and a longer reach before it opens up
    pointer_slow_factor = 0.5,
    pointer_full_speed_frac = 1.0,
    -- And how long the cursor is parked either side of a click, in seconds. Raise it
    -- if a click still moves the pointer; lower it if a click-drag feels stuck.
    press_freeze_secs = 0.12,
    -- How long every finger may stay on the pad and still count as a tap rather than a
    -- gesture, in seconds. How far they may travel is move_start_frac, the same threshold a
    -- pan arms at, so a contact either moves something or stays a tap and never both.
    --
    -- Used by the three-finger double tap, which brings the window under the cursor to you.
    -- Short on purpose: too long and a hand that lands, hesitates and lifts starts moving
    -- your view around.
    tap_max_secs = 0.25,
    -- And how far its fingers may drift and still be a tap, as a fraction of the pad.
    --
    -- Its own allowance rather than move_start_frac, which is a two-finger pan threshold and
    -- far too tight for three fingers. Measured on this pad: real three-finger taps drift 35
    -- to 316 units, swipes start around 960, and the pan's 81 sat in the middle of the taps,
    -- which is why half of them did not land. 0.06 is about 400 units, clear of both.
    tap_move_frac = 0.06,

    --
    -- Trackpad: two-finger gestures
    --

    -- Canvas units panned per device unit of finger travel, for a pan with two fingers or
    -- three. Three fingers pan and pinch the canvas whatever the keyboard is doing; two do the
    -- same only while Super is held, and otherwise scroll whatever is under the pointer.
    pan_sens = 0.12,
    -- Travel before a pan or a pinch does anything, as a fraction of the pad. The pan
    -- one is also how far a two-finger tap may drift and still count as a tap.
    move_start_frac = 0.012,
    pinch_start_frac = 0.02,
    -- How far three fingers must travel to count as a swipe, as a fraction of the pad.
    --
    -- A swipe goes to the nearest window in the direction it went, at whatever angle that is. It
    -- fires when you lift, not when you cross this, so going further is allowed and simply pulls
    -- the view further along: this is how far you must have gone by the time you let go.
    --
    -- Held to at least half again tap_move_frac, since a swipe has to want more travel than a tap
    -- is allowed to drift. Otherwise the same motion is legally both, and every three-finger tap
    -- draws a little slide before deciding it was not a swipe. The view also stays still until
    -- the fingers pass that drift, so a tap moves nothing at all.
    swipe_frac = 0.05,
    -- How much pan or pinch has to lead the other to take over, and the noise floor
    -- below which neither counts. Raise mode_bias to make the modes stickier.
    mode_bias = 1.6,
    mode_eps_frac = 0.0015,
    -- Pinch motion ignored around the current finger distance, so a pan does not
    -- wobble the zoom.
    pinch_deadzone_frac = 0.0005,
    -- While two fingers pan, the distance a pinch is measured against creeps toward where
    -- they actually are. Two fingers dragged across a pad drift apart on their own, and a
    -- distance remembered from where they landed eventually reads that drift as a pinch,
    -- which is a pan that zooms a little for no reason. Letting it creep forgives the
    -- drift; a real pinch is much faster and still counts.
    --
    -- As a fraction of the gap per frame:
    --   0      never creeps, which is what yuka did before this existed
    --   0.05   closes half the drift in about a fifth of a second
    --   0.2    forgives a lot of drift, but wants a brisk pinch to notice one
    zoom_ref_follow = 0.5,

    --
    -- Trackpad: how much contact counts as a touch
    --
    -- In the raw units the overlay's "size" line shows (this pad reports 0..2048).
    -- A contact smaller than touch_min_size is ignored completely: it does not point,
    -- does not count toward a gesture, and cannot decide a button. Once counted, it is
    -- only dropped when it falls under touch_drop_size, so a contact hovering around
    -- the threshold does not flicker in and out.
    --
    -- Both default to 0, which switches this off. Read your own numbers off the
    -- overlay first (Super+P, watch the size line for a fingertip against a graze),
    -- then set touch_min_size below the lightest touch you want to keep.
    --
    -- touch_drop_size must be under touch_min_size; if it is not, it becomes 70% of it.

    touch_min_size = 128,
    touch_drop_size = 0,

    --
    -- Trackpad: resting fingers
    --
    -- A hand rests on the pad. A finger that sits in the bottom of it and stops
    -- moving is parked: it does not steer the cursor, does not count toward the
    -- two-finger gesture test, and does not decide which button a click is. Both
    -- conditions are needed, or a slow-moving finger down there would be ignored.
    --
    -- The zone is measured up from the bottom edge, so 0.4 is the bottom 40%. Set it
    -- to 0 to switch the whole idea off.

    rest_zone_frac = 0.3,
    -- How long a finger must sit still to count as parked, in seconds.
    rest_secs = 0.1,
    -- And how far it has to move to be awake again, as a fraction of the pad.
    rest_move_frac = 0.008,

    --
    -- Trackpad: software buttons
    --
    -- Only used on a clickpad, where the whole surface is one button and there is no
    -- right button to read. Measured from the bottom edge, split across it.

    button_strip = 0.3333,
    button_split = 0.5,

    --
    -- Mouse
    --

    -- Mac-natural scrolling: content follows the fingers. Also decides the direction
    -- clients are told to scroll, so the canvas and the app agree.
    invert_scroll = true,
    -- Pixels a client is told to scroll per wheel notch.
    wheel_step_px = 15.0,
    -- Gap within which a second middle click counts as a double click, milliseconds.
    double_click_ms = 400,

    --
    -- Debugging
    --
    -- Settings rather than environment variables, so they can be turned on while yuka is
    -- running and off again without restarting it. Save the file and the next frame has them.
    --
    -- What they report is our reading of the pad rather than the raw event stream: raw events
    -- say what the kernel sent, these say what we made of it and why.

    -- Every contact, and whether it counted as a tap, with the numbers it was judged on.
    debug_taps = false,
    -- Any frame that moves the cursor further than a hand could have, with every slot's id and
    -- state at that instant. For the teleport that only happens sometimes.
    debug_jumps = false,

    --
    -- Windows
    --

    -- Smallest a Super+right-drag will ask a window to be, in canvas units. Clients
    -- that declared their own minimum through set_min_size keep it; this is the floor
    -- for the ones that did not. Whether a resize stretches the window while the client
    -- catches up is looks.settings.resize_stretch.
    resize_min_px = 120.0,
    -- How often the client is asked to re-render during a drag: the next ask waits for it
    -- to answer the last one, or for this many frames, whichever comes first. This does
    -- not affect how the corner tracks your cursor, only how fresh the pixels inside are.
    resize_wait_frames = 0,

    -- Whether a window being shrunk gets a mip chain, sampled trilinear with anisotropy.
    --
    -- It measurably reduces the shimmer on minified text. It used to be off because only
    -- textures we own can carry a chain, so half the canvas would have had it and half would
    -- not: a dmabuf sampled in place has one level, the client's own buffer, and there is
    -- nowhere to put the smaller ones. dmabuf_blit_below is what closes that gap.
    --
    -- false is also the way to spend nothing on this. It switches off the copy below as well,
    -- since that copy exists only to make a chain possible, so a machine that would rather have
    -- the memory and the fill rate than the sharpness turns off one key and pays for neither.
    mips_when_minified = true,

    -- The zoom below which a dmabuf window is copied into a texture of our own, so it can carry
    -- a chain like an shm one.
    --
    -- The copy is what a chain costs and this is when it is worth paying. At 1:1 and above
    -- nothing is being minified, so a chain buys nothing and sampling the client's buffer in
    -- place is free. Below this the window is being shrunk, which is where the shimmer lives.
    --
    -- Once per commit rather than per frame: a window that is not redrawing pays once and then
    -- nothing at all. A client redrawing every frame while you are zoomed out pays every frame,
    -- which is the case to watch. 0 switches it off entirely.
    dmabuf_blit_below = 0.9,

    -- What happens to a client's dmabuf once we have imported it. The reload is live, so the
    -- two can be compared inside one session by saving this file.
    --
    --   hold   Sample the client's buffer in place and keep it until a newer one arrives. No
    --          copy of any kind. Nothing tears, but the buffer is out of the client's rotation
    --          while we have it, and the texture is not ours so it cannot carry a mip chain.
    --   blit   Copy the imported image into a texture of our own, then hand the buffer back at
    --          once. One GPU side copy per commit: no CPU, no readback, nothing like the shm
    --          path. The client never waits on us, nothing tears, and the texture is ours, so
    --          dmabuf windows can carry a mip chain. Measured at about three times the slow
    --          frame rate on this machine, so it is a zoom quality trade rather than free.
    --
    -- A format that imports but is not colour renderable cannot be copied out of; blit falls
    -- back to hold for that window on its own.
    dmabuf_mode = "hold",

    --
    -- Camera
    --

    -- Zoom limits, and the scale the resets return to.
    zoom_min = 0.1,
    zoom_max = 8.0,
    zoom_default = 1.0,
    -- Perspective field of view in degrees: larger means more parallax when a window
    -- is lifted.
    fov_deg = 20.0,
}
