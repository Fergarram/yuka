# mt-bridge wire protocol

A tiny, count-prefixed stream of touch frames sent from the macOS host
(`mt-capture`) to the Linux guest (`mt-inject`) over TCP. One TCP connection,
guest connects to host. Both ends are little-endian aarch64, so structs are sent
raw with no byte swapping.

## Framing

Each touch frame is:

    u8   count            number of active contacts this frame (0..=16)
    u8   buttons          bit0 = left, bit1 = right (physical trackpad click)
    count x Contact       the contacts

A frame with `count = 0` means all fingers are up (an empty pad). Frames are sent
whenever the host trackpad reports a new contact frame, typically ~60-125 Hz.

## Contact (20 bytes, packed, little-endian)

    i32  id               stable per-finger id from the host; -1 is never used
    f32  x                normalized position, 0.0 = left,   1.0 = right
    f32  y                normalized position, 0.0 = bottom, 1.0 = top
    f32  size             contact size, roughly 0.0 .. 1.0 (finger flatness)
    u32  state            host contact state (see below)

`x`/`y` are normalized to the trackpad surface. The guest scales them to its
uinput axis range and flips Y (Linux MT Y grows downward).

## state

Mirrors MultitouchSupport's MTTouchState. The guest only distinguishes "on the
surface" (making/touching/lingering, roughly 3..=6) from "not" for deciding when
a slot's tracking id should go to -1.

    0 not tracking      1 starting        2 hovering       3 making contact
    4 touching          5 breaking        6 lingering      7 leaving

## buttons

The physical click is not in the MTTouch stream, so the host reads it with a
CGEventTap on the mouse buttons and puts the current state in every frame. A frame
is also sent the instant a button changes (using the last known contacts), so a
click registers even between touch frames.

## Notes

- No handshake or version byte yet: keep both ends in lockstep. Add a 4-byte
  magic + version if this ever ships beyond the dev VM.
