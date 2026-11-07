// Modified by Longbridge for gpui-fast.
//! GPU-written textures sampled directly by the ordinary sprite pipeline.
//!
//! The context owns the lookup table; the producer owns each entry through
//! `WgpuDynamicTexture`. No GPU objects enter GPUI's process-wide upload
//! backend, so browser handles stay on their browser thread too.

use std::{collections::HashMap, sync::Arc};

use anyhow::Result;
use gpui::{Bounds, DevicePixels, DynamicTexture, DynamicTextureId, Pixels, Size, Window};
use parking_lot::Mutex;

use crate::WgpuContext;

#[derive(Clone, Default)]
pub(crate) struct TextureStore(Arc<Mutex<HashMap<DynamicTextureId, wgpu::TextureView>>>);

impl TextureStore {
    pub(crate) fn view(&self, id: DynamicTextureId) -> Option<wgpu::TextureView> {
        self.0.lock().get(&id).cloned()
    }
}

/// An owned render target that GPUI samples without an upload or transfer copy.
///
/// Allocate it from the context drawing the window. Dropping it frees its
/// entry and retires its sprite id, so later sprite lookups skip it.
/// Notify the view when removing content, as for other view state changes.
/// A different context cannot resolve its id, even on the same adapter.
///
/// Check the creating context's `device_lost()` when handling device loss,
/// even if contents change rarely. Drop its targets promptly, before replacing
/// the context: each owner retains a context clone and its device. Recreate
/// targets from the replacement context and render their contents again.
/// `render` refuses a lost device; `paint` can still succeed for a stale owner,
/// but the replacement context skips its sprite and displays no texture.
pub struct WgpuDynamicTexture {
    context: WgpuContext,
    texture: DynamicTexture,
    format: wgpu::TextureFormat,
    srgb_view: Option<wgpu::TextureView>,
}

impl WgpuContext {
    /// Allocates a GPU-written dynamic texture on this context's device.
    ///
    /// The texture uses `color_texture_format()` and `RENDER_ATTACHMENT |
    /// TEXTURE_BINDING`. There is no initial pixel upload. Render new contents
    /// with `WgpuDynamicTexture::render` (or `render_srgb`), then paint with
    /// its `paint` method.
    ///
    /// Where the device can view a texture in another format, the texture
    /// also allows the sRGB variant of its format as a render view, so a
    /// producer can write linear color that is stored sRGB-encoded, as image
    /// bytes are. GPUI samples the plain view either way.
    pub fn create_dynamic_texture(&self, size: Size<DevicePixels>) -> Result<WgpuDynamicTexture> {
        anyhow::ensure!(!self.device_lost(), "dynamic texture device is lost");
        let limit = self.device.limits().max_texture_dimension_2d;
        anyhow::ensure!(
            size.width.0 > 0
                && size.height.0 > 0
                && size.width.0 as u32 <= limit
                && size.height.0 as u32 <= limit,
            "dynamic texture size must be positive and within the device's {limit} pixel limit"
        );
        let format = self.color_texture_format();
        let srgb_format = self.srgb_view_format(format);
        let target = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("dynamic_texture"),
            size: wgpu::Extent3d {
                width: size.width.0 as u32,
                height: size.height.0 as u32,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: srgb_format.as_slice(),
        });
        let srgb_view = srgb_format.map(|format| {
            target.create_view(&wgpu::TextureViewDescriptor {
                label: Some("dynamic_texture_srgb_view"),
                format: Some(format),
                ..Default::default()
            })
        });
        let texture = DynamicTexture::new_gpu(size)?;
        self.fast_dynamic_textures
            .0
            .lock()
            .insert(texture.id(), target.create_view(&Default::default()));
        Ok(WgpuDynamicTexture {
            context: self.clone(),
            texture,
            format,
            srgb_view,
        })
    }

    /// The sRGB view format a dynamic texture of `format` can be rendered
    /// through on this device, if any. GLES and WebGL cannot reinterpret a
    /// texture's format.
    fn srgb_view_format(&self, format: wgpu::TextureFormat) -> Option<wgpu::TextureFormat> {
        let srgb = format.add_srgb_suffix();
        (srgb != format
            && self
                .adapter
                .get_downlevel_capabilities()
                .flags
                .contains(wgpu::DownlevelFlags::VIEW_FORMATS)
            && self
                .adapter
                .get_texture_format_features(srgb)
                .allowed_usages
                .contains(wgpu::TextureUsages::RENDER_ATTACHMENT))
        .then_some(srgb)
    }
}

impl WgpuDynamicTexture {
    /// Writes new contents and, only after success, schedules a frame that
    /// draws again what painted the texture.
    ///
    /// Submit the producer's commands on the creating context's queue before
    /// returning. GPUI's later submission samples the same texture; queue
    /// ordering makes a CPU wait or transfer copy unnecessary. Do not submit
    /// partial writes and then return an error. Call only when contents change.
    /// Outside paint, success requests a redraw of the views and scroll-layer
    /// tiles that painted the texture (the whole window if none did yet).
    /// During paint it updates the current frame without scheduling another
    /// frame. Only this window is scheduled; notify other windows displaying
    /// the target so they redraw its new contents too.
    pub fn render<T>(
        &self,
        window: &mut Window,
        render: impl FnOnce(&wgpu::TextureView) -> Result<T>,
    ) -> Result<T> {
        let view = self.live_view()?;
        let result = render(&view)?;
        window.refresh_dynamic_texture(self.texture)?;
        Ok(result)
    }

    /// Like [`Self::render`], through the texture's sRGB view: shader output
    /// in linear color is stored sRGB-encoded, and GPUI composites it like an
    /// image. Fails where [`Self::srgb_view_format`] is `None`.
    pub fn render_srgb<T>(
        &self,
        window: &mut Window,
        render: impl FnOnce(&wgpu::TextureView) -> Result<T>,
    ) -> Result<T> {
        let srgb_view = self
            .srgb_view
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("this device cannot render through an sRGB view"))?;
        self.live_view()?;
        let result = render(srgb_view)?;
        window.refresh_dynamic_texture(self.texture)?;
        Ok(result)
    }

    /// The texture's format: the format of the view `render` passes and of
    /// the view GPUI samples.
    pub fn format(&self) -> wgpu::TextureFormat {
        self.format
    }

    /// The format of the view `render_srgb` passes, for building pipelines
    /// and multisampled attachments that resolve into it. `None` where the
    /// device cannot reinterpret texture formats (GLES, WebGL).
    pub fn srgb_view_format(&self) -> Option<wgpu::TextureFormat> {
        self.srgb_view
            .as_ref()
            .map(|_| self.format.add_srgb_suffix())
    }

    fn live_view(&self) -> Result<wgpu::TextureView> {
        anyhow::ensure!(
            !self.context.device_lost(),
            "dynamic texture device is lost"
        );
        self.context
            .fast_dynamic_textures
            .view(self.texture.id())
            .ok_or_else(|| anyhow::anyhow!("dynamic texture was removed"))
    }

    /// Paints with the window's current clipping, opacity and painter order.
    /// Call during paint in a window using the creating context.
    pub fn paint(&self, window: &mut Window, bounds: Bounds<Pixels>) -> Result<()> {
        window.paint_dynamic_texture(bounds, self.texture)
    }
}

impl Drop for WgpuDynamicTexture {
    fn drop(&mut self) {
        self.context
            .fast_dynamic_textures
            .0
            .lock()
            .remove(&self.texture.id());
        self.texture.remove();
    }
}

#[cfg(all(test, feature = "test-support", not(target_family = "wasm")))]
mod tests;
#[cfg(not(target_family = "wasm"))]
pub(crate) mod upload;
#[cfg(all(test, feature = "test-support", not(target_family = "wasm")))]
mod upload_tests;
