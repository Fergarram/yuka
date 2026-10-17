#version 100
#ifdef GL_FRAGMENT_PRECISION_HIGH
precision highp float;
#else
precision mediump float;
#endif

// The floor: a dot every so many canvas units, each a fixed number of pixels on screen. Drawn
// behind every window through the floor slot (lua/looks.lua, floor). frag_uv is the canvas
// point under this pixel, so the dots pan and zoom with the windows.
//
// Zoomed out until the dots crowd together, every other one fades away, so the grid doubles
// its spacing (24, 48, 96...) instead of turning to noise, and there is always a floor to see
// moving.

varying vec2 frag_uv;
varying vec4 frag_color;

// From the engine: canvas units to screen pixels.
uniform float canvasZoom;

// From the scripts.
uniform float spacing;   // canvas units between dots
uniform float dotPx;     // dot size on screen, in pixels
uniform vec4 dotColor;   // straight alpha
uniform float fadePx;    // the closest dots get on screen before every other one fades

// How much of this pixel a dot of a grid this many canvas units apart covers.
float dots(float step) {
    // From the nearest dot, in screen pixels.
    vec2 d = (fract(frag_uv / step + 0.5) - 0.5) * step * canvasZoom;
    vec2 cover = clamp(dotPx * 0.5 + 0.5 - abs(d), 0.0, 1.0);
    return cover.x * cover.y;
}

void main() {
    // The coarsest grid shown in full is the first doubling at least fadePx apart on screen.
    // The one finer than it is halfway through fading: whole at fadePx, gone at half of it.
    float doublings = max(0.0, ceil(log2(fadePx / (spacing * canvasZoom))));
    float coarse = spacing * exp2(doublings);
    float a = dots(coarse);
    if (doublings > 0.0) {
        float gap = coarse * 0.5 * canvasZoom;
        float t = clamp((gap - fadePx * 0.5) / (fadePx * 0.5), 0.0, 1.0);
        a = max(a, dots(coarse * 0.5) * t);
    }
    if (a <= 0.0) {
        discard;
    }
    gl_FragColor = vec4(dotColor.rgb, dotColor.a * a);
}
