//
// raylib FFI (thin wrapper)
//
// All raylib, rlgl and EGL C entry points live behind this module. Raw C types
// stay here; callers get plain Rust types. Grows as milestones need more.
//

use std::ffi::{c_char, c_int, c_void, CString};

//
// Constants
//

// rlgl pixel format for a 32 bit RGBA texture (bytes R,G,B,A).
pub const PIXELFORMAT_R8G8B8A8: i32 = 7;
// SetShaderValue uniform type for a single float.
pub const SHADER_UNIFORM_FLOAT: i32 = 0;

// raylib key codes we use for camera control.
pub const KEY_MINUS: i32 = 45;
pub const KEY_EQUAL: i32 = 61;
pub const KEY_A: i32 = 65;
pub const KEY_D: i32 = 68;
pub const KEY_S: i32 = 83;
pub const KEY_W: i32 = 87;
pub const KEY_KP_SUBTRACT: i32 = 333;
pub const KEY_KP_ADD: i32 = 334;
pub const KEY_LEFT_CONTROL: i32 = 341;
pub const KEY_RIGHT_CONTROL: i32 = 345;

//
// Types
//

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Rectangle {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Vector2 {
    pub x: f32,
    pub y: f32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Texture2D {
    pub id: u32,
    pub width: c_int,
    pub height: c_int,
    pub mipmaps: c_int,
    pub format: c_int,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Shader {
    pub id: u32,
    pub locs: *mut c_int,
}

pub const WHITE: Color = Color { r: 255, g: 255, b: 255, a: 255 };

//
// Extern
//

extern "C" {
    fn InitWindow(width: c_int, height: c_int, title: *const c_char);
    fn CloseWindow();
    fn WindowShouldClose() -> bool;
    fn BeginDrawing();
    fn EndDrawing();
    fn ClearBackground(color: Color);
    fn GetScreenWidth() -> c_int;
    fn GetScreenHeight() -> c_int;
    fn GetFrameTime() -> f32;
    fn IsKeyDown(key: c_int) -> bool;
    fn TakeScreenshot(file_name: *const c_char);

    fn DrawTexturePro(
        texture: Texture2D,
        source: Rectangle,
        dest: Rectangle,
        origin: Vector2,
        rotation: f32,
        tint: Color,
    );

    fn LoadShader(vs: *const c_char, fs: *const c_char) -> Shader;
    fn UnloadShader(shader: Shader);
    fn GetShaderLocation(shader: Shader, name: *const c_char) -> c_int;
    fn SetShaderValue(
        shader: Shader,
        loc: c_int,
        value: *const c_void,
        uniform_type: c_int,
    );
    fn BeginShaderMode(shader: Shader);
    fn EndShaderMode();

    // rlgl low level texture management.
    fn rlLoadTexture(
        data: *const c_void,
        width: c_int,
        height: c_int,
        format: c_int,
        mipmaps: c_int,
    ) -> u32;
    fn rlUpdateTexture(
        id: u32,
        offset_x: c_int,
        offset_y: c_int,
        width: c_int,
        height: c_int,
        format: c_int,
        data: *const c_void,
    );
    fn rlUnloadTexture(id: u32);

    // EGL, linked via -lEGL. The context is created by raylib's DRM platform.
    fn eglGetProcAddress(proc_name: *const c_char) -> *mut c_void;
    fn eglGetCurrentDisplay() -> *mut c_void;
    fn eglGetCurrentContext() -> *mut c_void;
}

//
// Window
//

pub fn init_window(width: i32, height: i32, title: &str) {
    let c_title = CString::new(title).unwrap();
    unsafe { InitWindow(width, height, c_title.as_ptr()) };
}

pub fn close_window() {
    unsafe { CloseWindow() };
}

pub fn window_should_close() -> bool {
    unsafe { WindowShouldClose() }
}

pub fn begin_drawing() {
    unsafe { BeginDrawing() };
}

pub fn end_drawing() {
    unsafe { EndDrawing() };
}

pub fn clear_background(color: Color) {
    unsafe { ClearBackground(color) };
}

pub fn screen_width() -> i32 {
    unsafe { GetScreenWidth() }
}

pub fn screen_height() -> i32 {
    unsafe { GetScreenHeight() }
}

pub fn frame_time() -> f32 {
    unsafe { GetFrameTime() }
}

pub fn is_key_down(key: i32) -> bool {
    unsafe { IsKeyDown(key) }
}

pub fn take_screenshot(path: &str) {
    let p = CString::new(path).unwrap();
    unsafe { TakeScreenshot(p.as_ptr()) };
}

//
// Textures
//

pub fn load_texture_rgba(data: *const u8, width: i32, height: i32) -> u32 {
    unsafe {
        rlLoadTexture(
            data as *const c_void,
            width,
            height,
            PIXELFORMAT_R8G8B8A8,
            1,
        )
    }
}

pub fn update_texture_rgba(id: u32, data: *const u8, width: i32, height: i32) {
    unsafe {
        rlUpdateTexture(
            id,
            0,
            0,
            width,
            height,
            PIXELFORMAT_R8G8B8A8,
            data as *const c_void,
        )
    };
}

pub fn unload_texture(id: u32) {
    unsafe { rlUnloadTexture(id) };
}

pub fn draw_texture_pro(
    texture: Texture2D,
    source: Rectangle,
    dest: Rectangle,
    tint: Color,
) {
    let origin = Vector2 { x: 0.0, y: 0.0 };
    unsafe { DrawTexturePro(texture, source, dest, origin, 0.0, tint) };
}

//
// Shaders
//

pub fn load_shader(vs_path: &str, fs_path: &str) -> Shader {
    let vs = CString::new(vs_path).unwrap();
    let fs = CString::new(fs_path).unwrap();
    unsafe { LoadShader(vs.as_ptr(), fs.as_ptr()) }
}

pub fn unload_shader(shader: Shader) {
    unsafe { UnloadShader(shader) };
}

pub fn shader_location(shader: Shader, name: &str) -> i32 {
    let n = CString::new(name).unwrap();
    unsafe { GetShaderLocation(shader, n.as_ptr()) }
}

pub fn set_shader_float(shader: Shader, loc: i32, value: f32) {
    unsafe {
        SetShaderValue(
            shader,
            loc,
            &value as *const f32 as *const c_void,
            SHADER_UNIFORM_FLOAT,
        )
    };
}

pub fn begin_shader_mode(shader: Shader) {
    unsafe { BeginShaderMode(shader) };
}

pub fn end_shader_mode() {
    unsafe { EndShaderMode() };
}

//
// EGL
//

pub fn proc_address(name: &str) -> *mut c_void {
    let n = CString::new(name).unwrap();
    unsafe { eglGetProcAddress(n.as_ptr()) }
}

pub fn egl_current_display() -> *mut c_void {
    unsafe { eglGetCurrentDisplay() }
}

pub fn egl_current_context() -> *mut c_void {
    unsafe { eglGetCurrentContext() }
}
