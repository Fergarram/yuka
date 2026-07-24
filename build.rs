//
// Build
//
// Builds the vendored raylib (third_party/raylib) for the DRM/KMS platform and
// links it statically. raylib's DRM platform selects a GLES2/EGL context and
// links EGL, GLESv2, drm and gbm, which is exactly the setup we need for
// zero-copy dmabuf import via EGLImage.
//

use std::env;
use std::path::PathBuf;

fn main() {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    let raylib_src = PathBuf::from(&manifest_dir).join("third_party/raylib");

    let dst = cmake::Config::new(&raylib_src)
        .define("PLATFORM", "DRM")
        .define("BUILD_EXAMPLES", "OFF")
        .define("BUILD_SHARED_LIBS", "OFF")
        .define("CMAKE_BUILD_TYPE", "Release")
        .define("CMAKE_INSTALL_LIBDIR", "lib")
        .build();

    println!("cargo:rustc-link-search=native={}/lib", dst.display());
    println!("cargo:rustc-link-lib=static=raylib");

    // System libraries raylib's DRM platform depends on.
    for lib in ["EGL", "GLESv2", "drm", "gbm", "m", "dl", "pthread", "atomic"] {
        println!("cargo:rustc-link-lib={}", lib);
    }

    println!("cargo:rerun-if-changed=build.rs");
}
