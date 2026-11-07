// Modified by Longbridge for gpui-fast.
mod cosmic_text_system;
mod fast;
mod wgpu_atlas;
mod wgpu_context;
mod wgpu_renderer;

pub use cosmic_text_system::*;
#[cfg(not(target_family = "wasm"))]
pub use fast::{
    adapter_preference::{WgpuAdapterPreference, describe_adapters, set_adapter_preference},
    app_context::{WgpuAppContext, WgpuContextGeneration, app_context},
    native_surface::WgpuSurfaceSource,
};
pub use fast::{dynamic_textures::WgpuDynamicTexture, renderer_options::WgpuTextRenderingParams};
pub use wgpu;
pub use wgpu_atlas::*;
pub use wgpu_context::*;
#[cfg(all(
    not(target_family = "wasm"),
    any(test, feature = "bench-support", feature = "test-support")
))]
pub use wgpu_renderer::WgpuHeadlessRenderer;
pub use wgpu_renderer::{GpuContext, WgpuRenderer, WgpuSurfaceConfig};
