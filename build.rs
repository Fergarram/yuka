//
// Build
//
// Builds the vendored raylib (third_party/raylib) and links it statically.
//
// Two platforms, chosen by the `windowed` feature:
//
//   DRM (default)  raylib owns the display: GLES2 on EGL, driving KMS directly. This
//                  is how om_wm runs for real.
//   Desktop        raylib is a Wayland client via GLFW, for the nested build used in
//                  development. Pinned to "ES 2.0" so the graphics path and the
//                  shaders (#version 100) are the same ones the DRM build uses;
//                  GLFW is built Wayland-only, since there is no X11 here.
//
// Either way the context is EGL, which is what zero-copy dmabuf import needs.
//

use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    let raylib_src = PathBuf::from(&manifest_dir).join("third_party/raylib");
    let windowed = env::var("CARGO_FEATURE_WINDOWED").is_ok();

    // Whether this raylib carries our patch for taking an already open KMS fd
    // (SetGraphicDeviceFd in rcore_drm.c). Detected rather than assumed: a raylib
    // update that drops the patch must not turn into a link error or, worse, a
    // silently ignored fd. Without it om_wm opens the card the old way, so this is a
    // cfg and not a build failure.
    let drm_platform = raylib_src.join("src/platforms/rcore_drm.c");
    let external_fd = fs::read_to_string(&drm_platform)
        .map(|src| src.contains("void SetGraphicDeviceFd(int fd)"))
        .unwrap_or(false);
    println!("cargo:rustc-check-cfg=cfg(raylib_external_fd)");
    if external_fd && !windowed {
        println!("cargo:rustc-cfg=raylib_external_fd");
    } else if !windowed {
        println!(
            "cargo:warning=vendored raylib has no SetGraphicDeviceFd patch: \
             om_wm will let raylib open the KMS device itself"
        );
    }

    let mut cfg = cmake::Config::new(&raylib_src);
    cfg.define("BUILD_EXAMPLES", "OFF")
        .define("BUILD_SHARED_LIBS", "OFF")
        .define("CMAKE_BUILD_TYPE", "Release")
        .define("CMAKE_INSTALL_LIBDIR", "lib");

    if windowed {
        cfg.define("PLATFORM", "Desktop")
            .define("OPENGL_VERSION", "ES 2.0")
            .define("GLFW_BUILD_WAYLAND", "ON")
            .define("GLFW_BUILD_X11", "OFF");
    } else {
        cfg.define("PLATFORM", "DRM");
    }

    let dst = cfg.build();

    println!("cargo:rustc-link-search=native={}/lib", dst.display());
    println!("cargo:rustc-link-lib=static=raylib");

    // Common to both: the EGL/GLES2 context, and what raylib always wants.
    let mut libs = vec!["EGL", "GLESv2", "m", "dl", "pthread", "atomic"];
    if windowed {
        // GLFW's Wayland backend.
        libs.extend(["wayland-client", "wayland-cursor", "wayland-egl", "xkbcommon"]);
    }
    // drm and gbm are raylib's on the DRM platform, and ours either way: cursor.rs
    // talks to libdrm directly, and it is compiled in both builds even though the
    // nested one never finds a card to use.
    libs.extend(["drm", "gbm"]);
    for lib in libs {
        println!("cargo:rustc-link-lib={}", lib);
    }

    println!("cargo:rerun-if-changed=build.rs");
    // The vendored raylib carries local patches, so changes there have to trigger a
    // rebuild. Without this, editing it silently changes nothing.
    println!("cargo:rerun-if-changed=third_party/raylib/src");
}
