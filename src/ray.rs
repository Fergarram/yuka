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
// rlgl primitive mode for quads.
pub const RL_QUADS: i32 = 0x0007;
// Camera projection: perspective.
pub const CAMERA_PERSPECTIVE: i32 = 0;


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
pub struct Vector2 {
    pub x: f32,
    pub y: f32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Vector3 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Camera3D {
    pub position: Vector3,
    pub target: Vector3,
    pub up: Vector3,
    pub fovy: f32,
    pub projection: c_int,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Ray {
    pub position: Vector3,
    pub direction: Vector3,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Shader {
    pub id: u32,
    pub locs: *mut c_int,
}

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
    fn SetExitKey(key: c_int);
    fn TakeScreenshot(file_name: *const c_char);

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

    // 3D mode + rlgl immediate-mode quad drawing.
    fn rlSetClipPlanes(near_plane: f64, far_plane: f64);
    fn BeginMode3D(camera: Camera3D);
    fn EndMode3D();
    fn GetScreenToWorldRay(position: Vector2, camera: Camera3D) -> Ray;
    fn rlSetTexture(id: u32);
    fn rlBegin(mode: c_int);
    fn rlEnd();
    fn rlColor4ub(r: u8, g: u8, b: u8, a: u8);
    fn rlTexCoord2f(x: f32, y: f32);
    fn rlVertex3f(x: f32, y: f32, z: f32);
    fn rlDisableBackfaceCulling();
    fn rlEnableBackfaceCulling();
    fn rlDisableDepthTest();
    fn rlEnableDepthTest();

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

pub fn set_exit_key(key: i32) {
    unsafe { SetExitKey(key) };
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

//
// 3D
//

// Override raylib's default 3D clip planes (0.01 .. 1000). Our perspective
// camera floats far above the canvas (height grows as we zoom out), so the
// default far plane clips every window. Depth test is off (painter's order), so
// the huge near/far range costs us no precision. Persists across BeginMode3D and
// GetScreenToWorldRay, so set it once at startup.
pub fn set_clip_planes(near_plane: f64, far_plane: f64) {
    unsafe { rlSetClipPlanes(near_plane, far_plane) };
}

pub fn begin_mode_3d(camera: Camera3D) {
    unsafe { BeginMode3D(camera) };
}

pub fn end_mode_3d() {
    unsafe { EndMode3D() };
}

pub fn screen_to_world_ray(x: f32, y: f32, camera: Camera3D) -> Ray {
    unsafe { GetScreenToWorldRay(Vector2 { x, y }, camera) }
}

pub fn disable_backface_culling() {
    unsafe { rlDisableBackfaceCulling() };
}

pub fn enable_backface_culling() {
    unsafe { rlEnableBackfaceCulling() };
}

pub fn disable_depth_test() {
    unsafe { rlDisableDepthTest() };
}

pub fn enable_depth_test() {
    unsafe { rlEnableDepthTest() };
}

// Draw a textured quad in 3D from four (position, uv) corners, in order.
pub fn draw_textured_quad(
    tex_id: u32,
    corners: [(Vector3, f32, f32); 4],
) {
    unsafe {
        rlSetTexture(tex_id);
        rlBegin(RL_QUADS);
        rlColor4ub(255, 255, 255, 255);
        for (v, u, w) in corners {
            rlTexCoord2f(u, w);
            rlVertex3f(v.x, v.y, v.z);
        }
        rlEnd();
        rlSetTexture(0);
    }
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
