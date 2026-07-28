//
// EGL import (Data Oriented zone)
//
// Turns client dmabufs into GL textures with zero copy: eglCreateImageKHR wraps
// the dmabuf planes as an EGLImage, then glEGLImageTargetTexture2DOES binds it
// to a GL_TEXTURE_2D that raylib samples. All raw pointers and unsafe stay in
// this module; callers get plain u32 texture ids and opaque image handles.
//
// Ported from waylandcraft/native/src/egl.rs, adapted to load entry points via
// raylib's rlGetProcAddress instead of glfwGetProcAddress.
//

use std::ffi::{c_char, c_void, CStr};

use crate::ray;

//
// Constants (EGL / GL enums)
//

const EGL_NONE: i32 = 0x3038;
const EGL_WIDTH: i32 = 0x3057;
const EGL_HEIGHT: i32 = 0x3056;
const EGL_LINUX_DMA_BUF_EXT: u32 = 0x3270;
const EGL_LINUX_DRM_FOURCC_EXT: i32 = 0x3271;

const EGL_DMA_BUF_PLANE_FD: [i32; 4] = [0x3272, 0x3275, 0x3278, 0x3440];
const EGL_DMA_BUF_PLANE_OFFSET: [i32; 4] = [0x3273, 0x3276, 0x3279, 0x3441];
const EGL_DMA_BUF_PLANE_PITCH: [i32; 4] = [0x3274, 0x3277, 0x327A, 0x3442];
const EGL_DMA_BUF_PLANE_MOD_LO: [i32; 4] = [0x3443, 0x3445, 0x3447, 0x3449];
const EGL_DMA_BUF_PLANE_MOD_HI: [i32; 4] = [0x3444, 0x3446, 0x3448, 0x344A];

const EGL_DEVICE_EXT: i32 = 0x322C;
const EGL_DRM_DEVICE_FILE_EXT: i32 = 0x3233;
const EGL_DRM_RENDER_NODE_FILE_EXT: i32 = 0x3377;

const GL_TEXTURE_2D: u32 = 0x0DE1;
const GL_TEXTURE_MIN_FILTER: u32 = 0x2801;
const GL_TEXTURE_MAG_FILTER: u32 = 0x2800;
const GL_TEXTURE_WRAP_S: u32 = 0x2802;
const GL_TEXTURE_WRAP_T: u32 = 0x2803;
const GL_NEAREST: i32 = 0x2600;
const GL_LINEAR: i32 = 0x2601;
const GL_LINEAR_MIPMAP_LINEAR: i32 = 0x2703;
const GL_CLAMP_TO_EDGE: i32 = 0x812F;
const GL_EXTENSIONS: u32 = 0x1F03;
const GL_NO_ERROR: u32 = 0;
// EXT_texture_filter_anisotropic.
const GL_TEXTURE_MAX_ANISOTROPY: u32 = 0x84FE;
const GL_MAX_TEXTURE_MAX_ANISOTROPY: u32 = 0x84FF;
// More than this buys nothing visible and costs bandwidth on a window that is only
// ever mildly oblique (ours tilt only in perspective, never edge-on).
const ANISOTROPY_CAP: f32 = 8.0;

//
// Types
//

pub type EglImage = *mut c_void;

pub struct DmabufPlane {
    pub fd: i32,
    pub offset: u32,
    pub stride: u32,
}

pub struct DmabufInfo {
    pub width: i32,
    pub height: i32,
    pub fourcc: u32,
    pub modifier: u64,
    pub has_modifier: bool,
    pub planes: Vec<DmabufPlane>,
}

type CreateImageFn =
    extern "C" fn(*mut c_void, *mut c_void, u32, *mut c_void, *const i32) -> EglImage;
type DestroyImageFn = extern "C" fn(*mut c_void, EglImage) -> u32;
type ImageTargetTextureFn = extern "C" fn(u32, EglImage);
type QueryFormatsFn =
    extern "C" fn(*mut c_void, i32, *mut i32, *mut i32) -> u32;
type QueryModifiersFn =
    extern "C" fn(*mut c_void, i32, i32, *mut u64, *mut u32, *mut i32) -> u32;
type QueryDisplayAttribFn = extern "C" fn(*mut c_void, i32, *mut isize) -> u32;
type QueryDeviceStringFn = extern "C" fn(*mut c_void, i32) -> *const c_char;

pub struct Egl {
    display: *mut c_void,
    create_image: CreateImageFn,
    destroy_image: DestroyImageFn,
    image_target_texture: ImageTargetTextureFn,
    query_formats: QueryFormatsFn,
    query_modifiers: QueryModifiersFn,
}

//
// GLES (linked directly via -lGLESv2)
//

extern "C" {
    fn glGenTextures(n: i32, textures: *mut u32);
    fn glBindTexture(target: u32, texture: u32);
    fn glTexParameteri(target: u32, pname: u32, param: i32);
    fn glTexParameterf(target: u32, pname: u32, param: f32);
    fn glDeleteTextures(n: i32, textures: *const u32);
    fn glGenerateMipmap(target: u32);
    fn glGetError() -> u32;
    fn glGetString(name: u32) -> *const c_char;
    fn glGetFloatv(name: u32, data: *mut f32);
}

//
// Texture sampling
//
// One policy for both upload paths, stated here rather than inherited: bilinear
// magnification, and trilinear plus anisotropic minification once a window has a mip
// chain. raylib's own default for the shm path was nearest on both, which is why a
// terminal used to go blocky under the same zoom that made a browser go smooth.
//
// Minification is where the quality actually goes. A window drawn at half scale reads
// one texel in four with no mip chain, so text crawls and shimmers as you pan. The
// chain is built lazily, only for windows that are really being minified, because
// generating one costs real GPU time per commit and a window at 1:1 or larger has no
// use for it.

// How much anisotropy the driver will give us, queried once. 1.0 means the extension
// is missing, which is also the value that disables it per texture.
pub fn max_anisotropy() -> f32 {
    let exts = unsafe { glGetString(GL_EXTENSIONS) };
    if exts.is_null() {
        return 1.0;
    }
    let exts = unsafe { std::ffi::CStr::from_ptr(exts) }.to_string_lossy();
    if !exts.contains("GL_EXT_texture_filter_anisotropic") {
        return 1.0;
    }
    let mut max: f32 = 1.0;
    unsafe { glGetFloatv(GL_MAX_TEXTURE_MAX_ANISOTROPY, &mut max) };
    max.clamp(1.0, ANISOTROPY_CAP)
}

// Bilinear both ways, no mip chain. What every window starts on.
pub fn set_filter_linear(tex: u32) {
    unsafe {
        glBindTexture(GL_TEXTURE_2D, tex);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
        glBindTexture(GL_TEXTURE_2D, 0);
    }
}

// Exact texels, no blending. Only correct when a window is drawn at 1:1 with its
// pixels on pixel boundaries, and then it is strictly better than bilinear: the sample
// point sits a hair off a texel centre in practice, and bilinear turns that into a
// two-texel blend, which is the faint blur on text at native scale.
pub fn set_filter_nearest(tex: u32) {
    unsafe {
        glBindTexture(GL_TEXTURE_2D, tex);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
        glBindTexture(GL_TEXTURE_2D, 0);
    }
}

// Trilinear + anisotropic on a texture that already has a chain, for going back to it
// after a spell at 1:1.
pub fn set_filter_trilinear(tex: u32, anisotropy: f32) {
    unsafe {
        glBindTexture(GL_TEXTURE_2D, tex);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR_MIPMAP_LINEAR);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
        if anisotropy > 1.0 {
            glTexParameterf(GL_TEXTURE_2D, GL_TEXTURE_MAX_ANISOTROPY, anisotropy);
        }
        glBindTexture(GL_TEXTURE_2D, 0);
    }
}

// Build a mip chain and switch this texture to trilinear + anisotropic. False means
// the driver refused, which is a real possibility for a dmabuf texture: level 0 of
// those is an EGLImage the client owns, and defining the smaller levels around it is
// not something every driver allows. The caller then leaves it on bilinear rather
// than asking again every frame.
pub fn build_mips(tex: u32, anisotropy: f32) -> bool {
    unsafe {
        glBindTexture(GL_TEXTURE_2D, tex);
        // Clear anything an earlier call left pending, so the check below is ours.
        while glGetError() != GL_NO_ERROR {}
        glGenerateMipmap(GL_TEXTURE_2D);
        if glGetError() != GL_NO_ERROR {
            glBindTexture(GL_TEXTURE_2D, 0);
            return false;
        }
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR_MIPMAP_LINEAR);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
        if anisotropy > 1.0 {
            glTexParameterf(GL_TEXTURE_2D, GL_TEXTURE_MAX_ANISOTROPY, anisotropy);
        }
        glBindTexture(GL_TEXTURE_2D, 0);
    }
    true
}

//
// Init
//

fn load<T>(name: &str) -> Option<T> {
    let p = ray::proc_address(name);
    if p.is_null() {
        eprintln!("om_wm: egl: missing entry point {name}");
        None
    } else {
        Some(unsafe { transmute_copy_fn::<T>(p) })
    }
}

// Reinterpret a void* proc address as a concrete fn pointer type.
unsafe fn transmute_copy_fn<T>(p: *mut c_void) -> T {
    debug_assert_eq!(
        std::mem::size_of::<T>(),
        std::mem::size_of::<*mut c_void>()
    );
    std::mem::transmute_copy::<*mut c_void, T>(&p)
}

pub fn init() -> Option<Egl> {
    let display = ray::egl_current_display();
    if display.is_null() {
        eprintln!("om_wm: egl: no current EGL display");
        return None;
    }

    Some(Egl {
        display,
        create_image: load("eglCreateImageKHR")?,
        destroy_image: load("eglDestroyImageKHR")?,
        image_target_texture: load("glEGLImageTargetTexture2DOES")?,
        query_formats: load("eglQueryDmaBufFormatsEXT")?,
        query_modifiers: load("eglQueryDmaBufModifiersEXT")?,
    })
}

//
// Format query
//

// Returns (fourcc, modifier) pairs importable as GL_TEXTURE_2D. Formats with no
// modifier list are reported with modifier 0 (linear-ish default advertisement).
impl Egl {
    pub fn query_formats(&self) -> Vec<(u32, u64)> {
        let mut count: i32 = 0;
        (self.query_formats)(self.display, 0, std::ptr::null_mut(), &mut count);
        if count <= 0 {
            return Vec::new();
        }

        let mut codes: Vec<i32> = vec![0; count as usize];
        (self.query_formats)(
            self.display,
            count,
            codes.as_mut_ptr(),
            &mut count,
        );
        codes.truncate(count.max(0) as usize);

        let mut out: Vec<(u32, u64)> = Vec::new();
        for code in codes {
            let mods = self.query_modifiers_for(code);
            if mods.is_empty() {
                out.push((code as u32, 0));
            } else {
                for m in mods {
                    out.push((code as u32, m));
                }
            }
        }
        out
    }

    fn query_modifiers_for(&self, format: i32) -> Vec<u64> {
        let mut count: i32 = 0;
        (self.query_modifiers)(
            self.display,
            format,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut count,
        );
        if count <= 0 {
            return Vec::new();
        }

        let mut mods: Vec<u64> = vec![0; count as usize];
        let mut external: Vec<u32> = vec![0; count as usize];
        (self.query_modifiers)(
            self.display,
            format,
            count,
            mods.as_mut_ptr(),
            external.as_mut_ptr(),
            &mut count,
        );
        let n = count.max(0) as usize;

        // Keep only modifiers importable as a normal (non external only) 2D texture.
        (0..n)
            .filter(|&i| external[i] == 0)
            .map(|i| mods[i])
            .collect()
    }

    //
    // Import
    //

    pub fn import_dmabuf(&self, info: &DmabufInfo) -> Option<(EglImage, u32)> {
        let mut attribs: Vec<i32> = Vec::with_capacity(32);
        attribs.push(EGL_WIDTH);
        attribs.push(info.width);
        attribs.push(EGL_HEIGHT);
        attribs.push(info.height);
        attribs.push(EGL_LINUX_DRM_FOURCC_EXT);
        attribs.push(info.fourcc as i32);

        for (i, plane) in info.planes.iter().enumerate().take(4) {
            attribs.push(EGL_DMA_BUF_PLANE_FD[i]);
            attribs.push(plane.fd);
            attribs.push(EGL_DMA_BUF_PLANE_OFFSET[i]);
            attribs.push(plane.offset as i32);
            attribs.push(EGL_DMA_BUF_PLANE_PITCH[i]);
            attribs.push(plane.stride as i32);
            if info.has_modifier {
                let lo = (info.modifier & 0xFFFF_FFFF) as i32;
                let hi = (info.modifier >> 32) as i32;
                attribs.push(EGL_DMA_BUF_PLANE_MOD_LO[i]);
                attribs.push(lo);
                attribs.push(EGL_DMA_BUF_PLANE_MOD_HI[i]);
                attribs.push(hi);
            }
        }
        attribs.push(EGL_NONE);

        let image = (self.create_image)(
            self.display,
            std::ptr::null_mut(), // EGL_NO_CONTEXT
            EGL_LINUX_DMA_BUF_EXT,
            std::ptr::null_mut(),
            attribs.as_ptr(),
        );
        if image.is_null() {
            return None;
        }

        let mut tex: u32 = 0;
        unsafe {
            glGenTextures(1, &mut tex);
            glBindTexture(GL_TEXTURE_2D, tex);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
        }
        (self.image_target_texture)(GL_TEXTURE_2D, image);
        unsafe { glBindTexture(GL_TEXTURE_2D, 0) };

        Some((image, tex))
    }

    // The render node's device number, for dmabuf feedback (tells clients which
    // GPU to allocate on). Queried from EGL, matching waylandcraft.
    pub fn render_node_dev(&self) -> Option<u64> {
        let query_attrib: QueryDisplayAttribFn =
            load("eglQueryDisplayAttribEXT")?;
        let query_string: QueryDeviceStringFn =
            load("eglQueryDeviceStringEXT")?;

        let mut device_attr: isize = 0;
        if query_attrib(self.display, EGL_DEVICE_EXT, &mut device_attr) == 0 {
            return None;
        }
        let device = device_attr as *mut c_void;

        let mut path_ptr = query_string(device, EGL_DRM_RENDER_NODE_FILE_EXT);
        if path_ptr.is_null() {
            path_ptr = query_string(device, EGL_DRM_DEVICE_FILE_EXT);
        }
        if path_ptr.is_null() {
            return None;
        }

        let path = unsafe { CStr::from_ptr(path_ptr) };
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::stat(path.as_ptr(), &mut st) } != 0 {
            return None;
        }
        println!(
            "om_wm: egl render node {} dev={:#x}",
            path.to_string_lossy(),
            st.st_rdev
        );
        Some(st.st_rdev as u64)
    }

    pub fn destroy(&self, image: EglImage, tex: u32) {
        if tex != 0 {
            unsafe { glDeleteTextures(1, &tex) };
        }
        if !image.is_null() {
            (self.destroy_image)(self.display, image);
        }
    }
}
