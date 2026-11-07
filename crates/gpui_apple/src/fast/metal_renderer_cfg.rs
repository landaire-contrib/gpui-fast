//! Which targets build the Metal renderer, decided by the build script.
//!
//! macOS windows draw through `gpui_wgpu`, so only iOS compiles the Metal
//! renderer and its shaders. The build script sets `fast_metal_renderer` for
//! iOS, and the crate gates the renderer's modules on it, so a macOS build
//! needs no Metal toolchain (`xcrun metal`) and can be checked from any host.

/// Declares the `fast_metal_renderer` cfg and sets it when the target draws
/// with Metal. Returns whether it did.
pub fn configure() -> bool {
    println!("cargo::rustc-check-cfg=cfg(fast_metal_renderer)");
    let enabled = std::env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "ios");
    if enabled {
        println!("cargo::rustc-cfg=fast_metal_renderer");
    }
    enabled
}
