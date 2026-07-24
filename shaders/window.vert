#version 100
precision mediump float;

// raylib standard vertex attributes and transform
attribute vec3 vertexPosition;
attribute vec2 vertexTexCoord;
attribute vec4 vertexColor;

uniform mat4 mvp;

varying vec2 frag_uv;
varying vec4 frag_color;

void main() {
    frag_uv = vertexTexCoord;
    frag_color = vertexColor;
    gl_Position = mvp * vec4(vertexPosition, 1.0);
}
