#version 100
#ifdef GL_FRAGMENT_PRECISION_HIGH
precision highp float;
#else
precision mediump float;
#endif

// The built-in window shader with rounded corners and a thin outline, drawn by us rather than
// the client (lua/looks.lua, corners). The corners round with the window as it zooms; the
// outline stays the same number of pixels on screen.
//
// windowParams: x corner radius in canvas units, y outline width in screen pixels, zw the
// window's own size. Popups and subsurfaces are drawn with their parent's shader and numbers,
// so only a surface the size of the window itself is rounded: the rest draw as they are.
//
// The window is windowRect inside the drawn quad, which is bigger when the client pads its
// buffer. Only the window is touched: padding outside it, where a client's shadow lives, is
// drawn as it is.

varying vec2 frag_uv;
varying vec4 frag_color;

uniform sampler2D texture0;
uniform vec4 colDiffuse;

uniform float alphaBlend;
uniform float swizzleBgra;
uniform float windowAlpha;
uniform vec4 windowParams;
uniform vec2 windowSize;
uniform vec4 windowRect;

// From the scripts: screen pixels per canvas unit, and the outline colour, straight alpha.
uniform float screenScale;
uniform vec4 outlineColor;

// Distance from a rounded rectangle's edge, negative inside: the rectangle is size big with its
// top-left at the origin, and its corners are circles of this radius.
float rounded_box(vec2 point, vec2 size, float radius) {
    vec2 half_size = size * 0.5;
    vec2 corner = abs(point - half_size) - (half_size - radius);
    return length(max(corner, 0.0)) + min(max(corner.x, corner.y), 0.0) - radius;
}

void main() {
    vec4 raw = texture2D(texture0, frag_uv);
    vec4 color = mix(raw, raw.bgra, swizzleBgra);

    // Wayland buffers carry premultiplied alpha; undo it, as the built-in shader does.
    if (color.a > 0.0) {
        color.rgb /= color.a;
    }
    color.a = color.a + alphaBlend * (1.0 - color.a);

    vec2 own_size = windowParams.zw;
    vec2 rect_size = windowRect.zw;
    vec2 point = frag_uv * windowSize - windowRect.xy;
    bool whole_window = all(lessThan(abs(rect_size - own_size), vec2(1.0)));
    bool in_rect = all(greaterThanEqual(point, vec2(0.0))) && all(lessThanEqual(point, rect_size));
    if (whole_window && in_rect) {
        float radius = min(windowParams.x, min(rect_size.x, rect_size.y) * 0.5);
        float pixel = 1.0 / max(screenScale, 0.0001);
        float edge = rounded_box(point, rect_size, radius);

        // A pixel wide ramp at the edge keeps the curve smooth.
        float inside = clamp(0.5 - edge / pixel, 0.0, 1.0);
        float band = clamp(0.5 + (edge + windowParams.y * pixel) / pixel, 0.0, 1.0);
        if (windowParams.y > 0.0) {
            // The outline lies over the window's own pixels, so it shows on any background
            // and over a client's transparent edges alike.
            float cover = band * outlineColor.a;
            color.rgb = mix(color.rgb, outlineColor.rgb, cover);
            color.a = max(color.a, cover);
        }
        color.a *= inside;
    }

    if (color.a == 0.0) {
        discard;
    }

    gl_FragColor = color * frag_color * colDiffuse;
    gl_FragColor.a *= windowAlpha;
}
