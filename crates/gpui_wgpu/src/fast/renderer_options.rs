//! Per-window settings a platform applies to its renderer: the clear color,
//! text rendering parameters from the OS and the surface color space.
//!
//! They live outside the renderer's device objects, so a device recovery,
//! which rebuilds those, applies them again.

use crate::WgpuRenderer;
use crate::wgpu_renderer::RendererState;

/// The font gamma used when `ZED_FONTS_GAMMA` is unset. Core Text coverage
/// is already gamma-adjusted, so Apple targets leave it alone.
pub(crate) const DEFAULT_FONTS_GAMMA: f32 = if cfg!(target_vendor = "apple") {
    1.0
} else {
    1.8
};

/// The grayscale text contrast used when
/// `ZED_FONTS_GRAYSCALE_ENHANCED_CONTRAST` is unset.
pub(crate) const DEFAULT_GRAYSCALE_ENHANCED_CONTRAST: f32 = if cfg!(target_vendor = "apple") {
    0.0
} else {
    1.0
};

/// Text antialiasing parameters, as an OS text stack reports them
/// (DirectWrite's `IDWriteRenderingParams`, for example).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WgpuTextRenderingParams {
    /// Alpha correction ratios, from `gpui::get_gamma_correction_ratios`.
    pub gamma_ratios: [f32; 4],
    pub grayscale_enhanced_contrast: f32,
    pub subpixel_enhanced_contrast: f32,
}

#[derive(Clone, Copy)]
pub(crate) struct RendererOptions {
    clear_color: wgpu::Color,
    text: Option<WgpuTextRenderingParams>,
    color_space: wgpu::SurfaceColorSpace,
}

impl Default for RendererOptions {
    fn default() -> Self {
        Self {
            clear_color: wgpu::Color::TRANSPARENT,
            text: None,
            color_space: wgpu::SurfaceColorSpace::Auto,
        }
    }
}

impl RendererOptions {
    pub(crate) fn clear_color(&self) -> wgpu::Color {
        self.clear_color
    }

    /// Applies `options` to `renderer`'s current device objects and surface.
    #[cfg(not(target_family = "wasm"))]
    pub(crate) fn apply(options: Self, renderer: &mut WgpuRenderer) {
        renderer.fast_options = options;
        renderer.apply_text_rendering_params();
        renderer.apply_color_space();
    }
}

impl WgpuRenderer {
    /// Sets the color each frame starts from. Transparent by default; an
    /// opaque window whose surface keeps a premultiplied alpha mode sets an
    /// opaque color here.
    pub fn set_clear_color(&mut self, color: wgpu::Color) {
        self.fast_options.clear_color = color;
    }

    /// Replaces the text rendering parameters read from the `ZED_FONTS_*`
    /// environment variables.
    pub fn set_text_rendering_params(&mut self, params: WgpuTextRenderingParams) {
        self.fast_options.text = Some(params);
        self.apply_text_rendering_params();
    }

    /// Requests a surface color space, such as `DisplayP3`. A color space
    /// the surface does not support for its format falls back to `Auto`.
    pub fn set_color_space(&mut self, color_space: wgpu::SurfaceColorSpace) {
        self.fast_options.color_space = color_space;
        self.apply_color_space();
    }

    fn apply_text_rendering_params(&mut self) {
        let Some(params) = self.fast_options.text else {
            return;
        };
        let (RendererState::Ready { core, .. } | RendererState::Unconfigured { core }) =
            &mut self.state
        else {
            return;
        };
        core.rendering_params.gamma_ratios = params.gamma_ratios;
        core.rendering_params.grayscale_enhanced_contrast = params.grayscale_enhanced_contrast;
        core.rendering_params.subpixel_enhanced_contrast = params.subpixel_enhanced_contrast;
    }

    fn apply_color_space(&mut self) {
        let RendererState::Ready { surface, core } = &self.state else {
            return;
        };
        let requested = self.fast_options.color_space;
        let supported = self.context.as_ref().is_some_and(|gpu_context| {
            gpu_context.borrow().as_ref().is_some_and(|context| {
                let format = self.surface_config.format;
                surface
                    .get_capabilities(&context.adapter)
                    .format_capabilities
                    .iter()
                    .find(|capabilities| capabilities.format == format)
                    .is_some_and(|capabilities| {
                        requested
                            .to_color_spaces()
                            .is_none_or(|spaces| capabilities.color_spaces.contains(spaces))
                    })
            })
        });
        let color_space = if supported {
            requested
        } else {
            if requested != wgpu::SurfaceColorSpace::Auto {
                log::warn!(
                    "Surface does not support color space {requested:?} for {:?}; using Auto",
                    self.surface_config.format
                );
            }
            wgpu::SurfaceColorSpace::Auto
        };
        if self.surface_config.color_space != color_space {
            self.surface_config.color_space = color_space;
            surface.configure(&core.resources.device, &self.surface_config);
        }
    }

    /// Renders `scene` with this window's device objects and atlas into an
    /// offscreen texture of the surface's size and reads it back.
    #[cfg(any(test, feature = "test-support"))]
    pub fn render_to_image(&mut self, scene: &gpui::Scene) -> anyhow::Result<image::RgbaImage> {
        let size = gpui::Size {
            width: gpui::DevicePixels(self.surface_config.width as i32),
            height: gpui::DevicePixels(self.surface_config.height as i32),
        };
        let premultiplied_alpha =
            self.surface_config.alpha_mode == wgpu::CompositeAlphaMode::PreMultiplied;
        let clear_color = self.fast_options.clear_color;
        let (RendererState::Ready { core, .. } | RendererState::Unconfigured { core }) =
            &mut self.state
        else {
            anyhow::bail!("Cannot render to an image: GPU resources have been released");
        };
        let format = core.target_format;
        anyhow::ensure!(
            matches!(
                format,
                wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Rgba8Unorm
            ),
            "Cannot read back surface format {format:?}"
        );
        let texture = core
            .resources
            .device
            .create_texture(&wgpu::TextureDescriptor {
                label: Some("render_to_image_target"),
                size: wgpu::Extent3d {
                    width: size.width.0 as u32,
                    height: size.height.0 as u32,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
        core.render_frame(
            scene,
            &texture.create_view(&wgpu::TextureViewDescriptor::default()),
            size,
            premultiplied_alpha,
            clear_color,
        )?;
        read_texture(&core.resources.device, &core.resources.queue, &texture)
    }
}

/// Copies a 4-byte-per-pixel BGRA8 or RGBA8 texture back to the CPU as RGBA.
#[cfg(any(test, feature = "test-support"))]
fn read_texture(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
) -> anyhow::Result<image::RgbaImage> {
    let width = texture.width();
    let height = texture.height();
    let bytes_per_row = width * 4;
    let padded_bytes_per_row = bytes_per_row.next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("render_to_image_readback"),
        size: u64::from(padded_bytes_per_row) * u64::from(height),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("render_to_image_readback"),
    });
    encoder.copy_texture_to_buffer(
        texture.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded_bytes_per_row),
                rows_per_image: Some(height),
            },
        },
        texture.size(),
    );
    let submission = queue.submit(std::iter::once(encoder.finish()));
    let (sender, receiver) = std::sync::mpsc::channel();
    buffer
        .slice(..)
        .map_async(wgpu::MapMode::Read, move |result| {
            sender.send(result).ok();
        });
    device
        .poll(wgpu::PollType::Wait {
            submission_index: Some(submission),
            timeout: Some(std::time::Duration::from_secs(30)),
        })
        .map_err(|error| anyhow::anyhow!("Failed to wait for rendering: {error}"))?;
    receiver
        .recv_timeout(std::time::Duration::from_secs(30))
        .map_err(|error| anyhow::anyhow!("Failed to receive the mapping result: {error}"))?
        .map_err(|error| anyhow::anyhow!("Failed to map the readback buffer: {error}"))?;

    let mapped = buffer.slice(..).get_mapped_range()?;
    let mut pixels = Vec::with_capacity(bytes_per_row as usize * height as usize);
    for row in mapped.chunks_exact(padded_bytes_per_row as usize) {
        pixels.extend_from_slice(&row[..bytes_per_row as usize]);
    }
    drop(mapped);
    buffer.unmap();
    if texture.format() == wgpu::TextureFormat::Bgra8Unorm {
        for pixel in pixels.chunks_exact_mut(4) {
            pixel.swap(0, 2);
        }
    }
    image::RgbaImage::from_raw(width, height, pixels)
        .ok_or_else(|| anyhow::anyhow!("Failed to create an image from the readback"))
}
