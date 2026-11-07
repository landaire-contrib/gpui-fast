//! The CPU upload backend of `Window::create_dynamic_texture` on wgpu.
//!
//! Each texture is its own wgpu texture on the device of the published
//! window context (`crate::app_context`), registered in that context's
//! lookup table, so the frame resolves it like a GPU-written dynamic texture.
//! `Queue::write_texture` is ordered before the queue's next submission, so a
//! frame never samples a half-written region and needs no staging of its own.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use anyhow::{Context as _, Result};
use gpui::{Bounds, DevicePixels, DynamicTextureId, DynamicTexturePlatform, Size};
use parking_lot::Mutex;

use crate::WgpuContext;

/// Installed as GPUI's upload platform when the first context is published.
static BACKEND: LazyLock<Arc<UploadTextures>> = LazyLock::new(|| {
    let backend = Arc::new(UploadTextures::default());
    gpui::set_dynamic_texture_platform(backend.clone());
    backend
});

/// Makes `context` the device CPU dynamic textures live on, installing the
/// backend the first time.
///
/// Textures created on an earlier device are recreated on this one at the
/// same size, under the same ids. Their contents were on the lost device and
/// read as transparent until the application uploads them again.
pub(crate) fn use_context(context: &WgpuContext) {
    BACKEND.state.lock().use_context(context);
}

#[derive(Default)]
struct UploadTextures {
    state: Mutex<UploadState>,
}

#[derive(Default)]
struct UploadState {
    context: Option<WgpuContext>,
    textures: HashMap<DynamicTextureId, UploadTexture>,
}

struct UploadTexture {
    texture: wgpu::Texture,
    size: Size<DevicePixels>,
}

impl UploadState {
    fn use_context(&mut self, context: &WgpuContext) {
        if let Some(current) = &self.context {
            if current.device.as_ref() == context.device.as_ref() {
                return;
            }
            let mut views = current.fast_dynamic_textures.0.lock();
            for id in self.textures.keys() {
                views.remove(id);
            }
        }
        if !self.textures.is_empty() {
            log::info!(
                "Recreating {} CPU dynamic textures on a new GPU device; their contents must be uploaded again",
                self.textures.len()
            );
        }
        for (id, upload) in &mut self.textures {
            upload.texture = create_texture(context, upload.size);
            register(context, *id, &upload.texture);
        }
        self.context = Some(context.clone());
    }

    fn context(&self) -> Result<&WgpuContext> {
        let context = self
            .context
            .as_ref()
            .context("no wgpu context has been published yet")?;
        anyhow::ensure!(!context.device_lost(), "the GPU device is lost");
        Ok(context)
    }
}

fn create_texture(context: &WgpuContext, size: Size<DevicePixels>) -> wgpu::Texture {
    context.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("cpu_dynamic_texture"),
        size: wgpu::Extent3d {
            width: size.width.0 as u32,
            height: size.height.0 as u32,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: context.color_texture_format(),
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    })
}

fn register(context: &WgpuContext, id: DynamicTextureId, texture: &wgpu::Texture) {
    context
        .fast_dynamic_textures
        .0
        .lock()
        .insert(id, texture.create_view(&Default::default()));
}

/// Writes tightly packed BGRA8 rows into `bounds` of `texture`.
fn write(
    context: &WgpuContext,
    texture: &wgpu::Texture,
    bounds: Bounds<DevicePixels>,
    bytes: &[u8],
) {
    let width = bounds.size.width.0 as u32;
    let height = bounds.size.height.0 as u32;
    // Atlas textures fall back to RGBA8 on adapters without BGRA8 sampling.
    let swizzled;
    let data = if texture.format() == wgpu::TextureFormat::Rgba8Unorm {
        swizzled = crate::wgpu_atlas::swizzle_upload_data(bytes, texture.format());
        &swizzled[..]
    } else {
        bytes
    };
    context.queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d {
                x: bounds.origin.x.0 as u32,
                y: bounds.origin.y.0 as u32,
                z: 0,
            },
            aspect: wgpu::TextureAspect::All,
        },
        data,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(width * 4),
            rows_per_image: Some(height),
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
}

impl DynamicTexturePlatform for UploadTextures {
    fn create_texture(
        &self,
        id: DynamicTextureId,
        size: Size<DevicePixels>,
        bytes: &[u8],
    ) -> Result<()> {
        let mut state = self.state.lock();
        let context = state.context()?;
        let limit = context.device.limits().max_texture_dimension_2d;
        anyhow::ensure!(
            size.width.0 as u32 <= limit && size.height.0 as u32 <= limit,
            "dynamic texture size exceeds the device's {limit} pixel limit"
        );
        let texture = create_texture(context, size);
        let bounds = Bounds {
            origin: gpui::point(DevicePixels(0), DevicePixels(0)),
            size,
        };
        write(context, &texture, bounds, bytes);
        register(context, id, &texture);
        state.textures.insert(id, UploadTexture { texture, size });
        Ok(())
    }

    fn update_texture(
        &self,
        id: DynamicTextureId,
        bounds: Bounds<DevicePixels>,
        bytes: &[u8],
    ) -> Result<()> {
        let state = self.state.lock();
        let context = state.context()?;
        let upload = state
            .textures
            .get(&id)
            .context("unknown CPU dynamic texture")?;
        write(context, &upload.texture, bounds, bytes);
        Ok(())
    }

    fn remove_texture(&self, id: DynamicTextureId) {
        let mut state = self.state.lock();
        if state.textures.remove(&id).is_some()
            && let Some(context) = &state.context
        {
            context.fast_dynamic_textures.0.lock().remove(&id);
        }
    }
}
