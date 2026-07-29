# om_wm devlog screenshots

Frames captured while working, 2026-07-28 and 2026-07-29. All are full 1440x900
DRM captures unless the name says crop; the crops are magnified regions of the
frame with the matching number, kept because the detail is unreadable at 1:1.

Some frames were taken with a temporary harness (forced zoom, forced settings,
fabricated trackpad contacts) that is not in the tree. Those are noted.

| file | what it shows |
| --- | --- |
| 01-nested-mode-under-weston | the nested build (`--features windowed`) running inside headless weston, four clients mapped |
| 02-card-fd-through-libseat | first DRM run after logind opens the card and raylib takes our fd |
| 03-filtering-first-run | the canvas after the sampling policy landed, at 1:1 |
| 04/05-zoom0.4-no-mips / mips | same scene zoomed out, without and with mip chains (four clients, so placement differs between runs) |
| 06/07-zoom0.4-...-single-client | the same comparison with one client, so the two frames are the same scene |
| 08/09-crop-text-no-mips / mips | terminal text from 06/07 magnified 5x. Without a chain, glyph strokes lose chunks; with one they hold shape and soften. Mean neighbour delta 4.86 vs 3.02 |
| 10-chromium-vanishes-zoomed-out | the dmabuf mip bug: Chromium imported zero-copy and then wasn't drawn at all at 0.4x, while shm windows were. Forced zoom |
| 11-chromium-renders-after-fix | same run after restricting mip chains to textures we own |
| 12-debug-labels-with-chromium | the per-window labels, including `window dmabuf ... bilinear (no mips on dmabuf)` |
| 13-nearest-at-1to1-one-window-bilinear | nearest at 1:1 working, and the 806x491 window still on bilinear because the cascade had placed it on a half unit |
| 14-nearest-after-placement-rounding | same scene after rounding placement: all three windows on nearest |
| 14b-crop-nearest-label | that window's label, magnified: `nearest scale 1.00x` |
| 15-policy-after-zoom-reset | after a forced pan at 0.37x and a zoom reset, everything back on nearest. Temporary harness |
| 16-chromium-without-decorations | xdg-decoration forced to server-side: square corners, no window buttons. Reverted afterwards, since it made no difference on Fernando's machine |
| 17-trackpad-overlay (+17b) | the trackpad instrument: surface at true aspect, button regions, live gesture state |
| 18-trackpad-overlay-resting-zone (+18b) | same, with the resting zone line and the `N resting` counter |
| 19-trackpad-overlay-contact-size-line (+19b) | same, with the `size ... load n/256` line |
| 20-contact-footprints-fingertip-vs-thumb (+20b) | contact ellipses drawn at true scale: a pointing fingertip (major 300) against a resting thumb (major 900). Fabricated contacts, since a screenshot cannot include my fingers |
