//! The headless renderer each platform offers tests and benchmarks.
//!
//! It is `gpui_wgpu`'s wherever wgpu runs natively: Linux and macOS windows
//! draw through `gpui_wgpu` too, and on Windows it draws on DX12.

use gpui::PlatformHeadlessRenderer;

/// `current_headless_renderer`: `gpui_wgpu`'s headless renderer on Linux,
/// macOS and Windows, `None` elsewhere.
pub(crate) fn current_headless_renderer()
-> anyhow::Result<Option<Box<dyn PlatformHeadlessRenderer>>> {
    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    {
        gpui_wgpu::WgpuHeadlessRenderer::new()
            .map(|renderer| Some(Box::new(renderer) as Box<dyn PlatformHeadlessRenderer>))
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        Ok(None)
    }
}
