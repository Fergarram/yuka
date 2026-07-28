//
// raylib FFI (thin wrapper)
//
// All raylib, rlgl and EGL C entry points live behind this module. Raw C types
// stay here; callers get plain Rust types. Grows as milestones need more.
//

use std::ffi::{c_char, c_int, c_uint, c_void, CString};

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
    fn SetConfigFlags(flags: c_uint);
    // GLFW is linked inside raylib, and init hints set before raylib calls glfwInit
    // are honoured.
    // Host input, for the nested build: the compositor we run inside owns the
    // devices and hands these to us through GLFW.
    fn GetMousePosition() -> Vector2;
    fn GetMouseDelta() -> Vector2;
    fn GetMouseWheelMoveV() -> Vector2;
    fn IsMouseButtonPressed(button: c_int) -> bool;
    fn IsMouseButtonReleased(button: c_int) -> bool;
    fn IsMouseButtonDown(button: c_int) -> bool;
    fn IsKeyPressed(key: c_int) -> bool;
    fn IsKeyReleased(key: c_int) -> bool;
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
    fn GetWorldToScreen(position: Vector3, camera: Camera3D) -> Vector2;
    fn rlDrawRenderBatchActive();
    fn DrawText(text: *const c_char, x: c_int, y: c_int, size: c_int, color: Color);
    fn MeasureText(text: *const c_char, size: c_int) -> c_int;
    fn DrawRectangle(x: c_int, y: c_int, w: c_int, h: c_int, color: Color);
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

// FLAG_WINDOW_UNDECORATED: a nested canvas wants no host titlebar around it.
pub const FLAG_WINDOW_UNDECORATED: u32 = 0x0000_0008;
// FLAG_VSYNC_HINT: nested, nothing paces us, so ask GLFW for a swap interval of 1
// and let the host's frame callbacks set our frame rate.
pub const FLAG_VSYNC_HINT: u32 = 0x0000_0040;

// GLFW loads libdecor at init to draw client-side decorations, and the only libdecor
// plugin installed here is the GTK one, which initialises GDK. Against a host with no
// keyboard capability (headless weston) GDK asserts and the process dies before a
// window ever exists. We draw our own everything anyway, so tell GLFW to skip it.
//
// KMS device handoff, in the DRM build only
//
// SetGraphicDeviceFd is our patch to raylib's DRM platform (rcore_drm.c). build.rs
// looks for it in the vendored source and sets raylib_external_fd, so a raylib that
// does not carry the patch still builds: can_set_drm_fd() then answers false and the
// caller lets raylib open the card itself, the way it did before.

#[cfg(raylib_external_fd)]
extern "C" {
    fn SetGraphicDeviceFd(fd: c_int);
}

// Whether this build can be handed a KMS fd at all.
pub fn can_set_drm_fd() -> bool {
    cfg!(raylib_external_fd)
}

// Hand raylib an open KMS device to use instead of opening one. Must be called
// before init_window, which is the only time raylib reads it. The fd stays ours to
// close.
#[cfg(raylib_external_fd)]
pub fn set_drm_fd(fd: i32) {
    unsafe { SetGraphicDeviceFd(fd) };
}

#[cfg(not(raylib_external_fd))]
pub fn set_drm_fd(_fd: i32) {}

//
// GLFW, in the nested build only
//
// raylib links GLFW for its desktop platform and not for DRM, so these two are
// cfg'd rather than merely unused: calling them in a DRM build would be a missing
// symbol at link time.

#[cfg(feature = "windowed")]
extern "C" {
    fn glfwInitHint(hint: c_int, value: c_int);
    // GLFW's own scancode for a key. Its Wayland backend, the only one the nested
    // build compiles, builds that table straight out of linux/input-event-codes.h,
    // so the scancode IS the evdev code. This is what lets host keys reach clients
    // without a hand-written key table of our own.
    fn glfwGetKeyScancode(key: c_int) -> c_int;
}

#[cfg(feature = "windowed")]
const GLFW_WAYLAND_LIBDECOR: c_int = 0x0005_3001;
#[cfg(feature = "windowed")]
const GLFW_WAYLAND_DISABLE_LIBDECOR: c_int = 0x0003_8002;

#[cfg(feature = "windowed")]
pub fn disable_libdecor() {
    unsafe { glfwInitHint(GLFW_WAYLAND_LIBDECOR, GLFW_WAYLAND_DISABLE_LIBDECOR) };
}

#[cfg(not(feature = "windowed"))]
pub fn disable_libdecor() {}

pub fn set_config_flags(flags: u32) {
    unsafe { SetConfigFlags(flags) };
}

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

// Where a canvas point lands on screen, through the same perspective camera the
// windows are drawn with. Debug labels are drawn in 2D at these points, so they follow
// the pan and the lift exactly.
pub fn world_to_screen(position: Vector3, camera: Camera3D) -> Vector2 {
    unsafe { GetWorldToScreen(position, camera) }
}

// Push whatever 2D geometry is still queued to the framebuffer. rlgl batches it and
// normally flushes at EndDrawing, which is after take_screenshot reads the pixels: any
// text or rectangle drawn this frame would be missing from the file.
pub fn flush_batch() {
    unsafe { rlDrawRenderBatchActive() };
}

pub fn draw_rectangle(x: i32, y: i32, w: i32, h: i32, color: Color) {
    unsafe { DrawRectangle(x, y, w, h, color) };
}

pub fn draw_text(text: &str, x: i32, y: i32, size: i32, color: Color) {
    let c = match CString::new(text) {
        Ok(c) => c,
        Err(_) => return,
    };
    unsafe { DrawText(c.as_ptr(), x, y, size, color) };
}

pub fn measure_text(text: &str, size: i32) -> i32 {
    let c = match CString::new(text) {
        Ok(c) => c,
        Err(_) => return 0,
    };
    unsafe { MeasureText(c.as_ptr(), size) }
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
// Host input (nested build)
//

pub fn mouse_position() -> (i32, i32) {
    let p = unsafe { GetMousePosition() };
    (p.x as i32, p.y as i32)
}

pub fn mouse_delta() -> (f32, f32) {
    let d = unsafe { GetMouseDelta() };
    (d.x, d.y)
}

// Wheel movement this frame: (horizontal, vertical), positive up and right, which
// is the convention the rest of om_wm uses.
pub fn mouse_wheel() -> (f32, f32) {
    let w = unsafe { GetMouseWheelMoveV() };
    (w.x, w.y)
}

pub fn mouse_pressed(button: i32) -> bool {
    unsafe { IsMouseButtonPressed(button) }
}

pub fn mouse_released(button: i32) -> bool {
    unsafe { IsMouseButtonReleased(button) }
}

pub fn mouse_down(button: i32) -> bool {
    unsafe { IsMouseButtonDown(button) }
}

pub fn key_pressed(key: i32) -> bool {
    unsafe { IsKeyPressed(key) }
}

pub fn key_released(key: i32) -> bool {
    unsafe { IsKeyReleased(key) }
}

// The evdev keycode behind a GLFW key, or None for the keys this platform has no
// code for (the table is filled with -1 and only the known keys are written). GLFW's
// Wayland backend keys its table by evdev code, so no arithmetic is needed here;
// its X11 backend would report evdev+8, and we do not build that one.
#[cfg(feature = "windowed")]
pub fn key_to_evdev(key: i32) -> Option<u16> {
    let scancode = unsafe { glfwGetKeyScancode(key) };
    if scancode > 0 {
        Some(scancode as u16)
    } else {
        None
    }
}

#[cfg(not(feature = "windowed"))]
pub fn key_to_evdev(_key: i32) -> Option<u16> {
    None
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
