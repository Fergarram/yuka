#version 100
precision mediump float;

varying vec2 frag_uv;
varying vec4 frag_color;

// raylib standard sampler + tint
uniform sampler2D texture0;
uniform vec4 colDiffuse;

// per window effect parameter (ported from waylandcraft window_info)
uniform float alphaBlend;
// 1.0 for shm buffers (BGRA in memory), 0.0 for dmabuf (correct RGBA via EGL)
uniform float swizzleBgra;

void main() {
    vec4 raw = texture2D(texture0, frag_uv);
    // shm uploads raw ARGB8888 bytes (B,G,R,A) as RGBA, so swizzle those back.
    vec4 color = mix(raw, raw.bgra, swizzleBgra);

    color.a = color.a + alphaBlend * (1.0 - color.a);
    if (color.a == 0.0) {
        discard;
    }

    gl_FragColor = color * frag_color * colDiffuse;
}
