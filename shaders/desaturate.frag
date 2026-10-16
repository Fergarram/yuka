#version 100
precision mediump float;

// The built-in window shader plus one effect: drain the colour out of a window by
// windowParams.x, 0 for none and 1 for grey. An example of a shader the scripts load into a
// slot and point windows at (lua/looks.lua, unfocused_desaturate).

varying vec2 frag_uv;
varying vec4 frag_color;

uniform sampler2D texture0;
uniform vec4 colDiffuse;

uniform float alphaBlend;
uniform float swizzleBgra;
uniform float windowAlpha;
uniform vec4 windowParams;

void main() {
    vec4 raw = texture2D(texture0, frag_uv);
    vec4 color = mix(raw, raw.bgra, swizzleBgra);

    // Wayland buffers carry premultiplied alpha; undo it, as the built-in shader does.
    if (color.a > 0.0) {
        color.rgb /= color.a;
    }

    color.a = color.a + alphaBlend * (1.0 - color.a);
    if (color.a == 0.0) {
        discard;
    }

    float grey = dot(color.rgb, vec3(0.299, 0.587, 0.114));
    color.rgb = mix(color.rgb, vec3(grey), clamp(windowParams.x, 0.0, 1.0));

    gl_FragColor = color * frag_color * colDiffuse;
    gl_FragColor.a *= windowAlpha;
}
