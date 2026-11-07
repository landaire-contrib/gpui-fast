//! Everything gpui-fast adds to the wgpu renderer lives under this module.
//!
//! The rest of this crate is kept as close to upstream GPUI (Zed's
//! `crates/gpui_wgpu`) as possible, so that pulling in upstream changes stays
//! a matter of merging rather than of rewriting. Upstream files only *call
//! into* this module: a field holding this module's state, a line forwarding a
//! call to it. The logic itself — data structures, algorithms, bookkeeping,
//! tests — is written here, one file per topic.

#[cfg(not(target_family = "wasm"))]
pub(crate) mod adapter_preference;
#[cfg(not(target_family = "wasm"))]
pub(crate) mod app_context;
pub(crate) mod atlas;
pub(crate) mod bind_groups;
#[cfg(not(target_family = "wasm"))]
pub(crate) mod composition;
pub(crate) mod dynamic_textures;
pub(crate) mod frame;
pub(crate) mod globals;
pub(crate) mod layers;
#[cfg(not(target_family = "wasm"))]
pub(crate) mod native_surface;
#[cfg(any(target_os = "macos", target_os = "ios"))]
pub(crate) mod paint_surface;
/// Only Apple scenes carry surfaces (`PaintSurface` holds a CVPixelBuffer
/// there), so elsewhere a batch of them has nothing to draw.
#[cfg(not(any(target_os = "macos", target_os = "ios")))]
pub(crate) mod paint_surface {
    pub(crate) fn draw_core(
        _core: &crate::wgpu_renderer::WgpuRendererCore,
        _scene: &gpui::Scene,
        _range: std::ops::Range<usize>,
        _pass: &mut wgpu::RenderPass<'_>,
    ) {
    }
}
pub(crate) mod pass_state;
pub(crate) mod renderer_options;
pub(crate) mod rounded_clip;
#[cfg(all(test, not(target_family = "wasm")))]
pub(crate) mod test_gpu;
