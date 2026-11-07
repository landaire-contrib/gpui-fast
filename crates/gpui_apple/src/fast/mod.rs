//! Everything gpui-fast adds to the Metal renderer and text system lives under
//! this module.
//!
//! The rest of this crate is kept as close to upstream GPUI (Zed's
//! `crates/gpui_apple`) as possible, so that pulling in upstream changes stays
//! a matter of merging rather than of rewriting. Upstream files only *call
//! into* this module: a field holding this module's state, a line forwarding a
//! call to it. The logic itself — data structures, algorithms, bookkeeping,
//! tests — is written here, one file per topic.
//!
//! The Metal renderer and everything it draws with build only for iOS
//! (`fast_metal_renderer`, set by `metal_renderer_cfg` in the build script):
//! macOS windows draw through `gpui_wgpu`.

#[cfg(fast_metal_renderer)]
pub(crate) mod dynamic_textures;
#[cfg(fast_metal_renderer)]
pub(crate) mod layers;
#[cfg(fast_metal_renderer)]
pub(crate) mod paths;
#[cfg(fast_metal_renderer)]
pub(crate) mod rounded_clip;
#[cfg(feature = "font-kit")]
pub(crate) mod text_system;
