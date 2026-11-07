// Modified by Longbridge for gpui-fast.
//! The CPU upload backend of `Window::create_dynamic_texture` on wgpu.
//!
//! Each texture is its own wgpu texture on the device of the published
//! window context (`crate::app_context`), registered in that context's
//! lookup table, so the frame resolves it like a GPU-written dynamic texture.
//!
//! Writes go into a CPU copy of each texture and mark a dirty region; the
//! next frame on the device uploads the dirty region with
//! `Queue::write_texture` just before it submits. wgpu stages every
//! `write_texture` until a submission, and a hidden or occluded window
//! submits nothing, so writing straight to the queue would stage every frame
//! a producer streams while no window draws. Coalesced into the copy, any
//! number of writes between frames cost one upload of what they changed.
//! The copy also restores the contents on a new device after device loss.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};

use anyhow::{Context as _, Result};
use gpui::{Bounds, DevicePixels, DynamicTextureId, DynamicTexturePlatform, Size, point};
use parking_lot::Mutex;

use crate::WgpuContext;

/// Installed as GPUI's upload platform when the first context is published.
static BACKEND: LazyLock<Arc<UploadTextures>> = LazyLock::new(|| {
    let backend = Arc::new(UploadTextures::default());
    gpui::set_dynamic_texture_platform(backend.clone());
    backend
});

/// Whether any texture holds a write not uploaded yet, so a frame with
/// nothing to upload skips the backend's lock.
static PENDING: AtomicBool = AtomicBool::new(false);

/// Makes `context` the device CPU dynamic textures live on, installing the
/// backend the first time.
///
/// Textures created on an earlier device are recreated on this one at the
/// same size, under the same ids, and their contents are uploaded again by
/// the next frame.
pub(crate) fn use_context(context: &WgpuContext) {
    BACKEND.state.lock().use_context(context);
}

/// Moves CPU dynamic textures off the published device, keeping their
/// contents for the next one. Creating a texture fails until then.
pub(crate) fn release() {
    BACKEND.state.lock().release();
}

/// Uploads the writes made since the last frame on `device`. Called by every
/// frame before it submits, so the uploads are ordered before its sampling.
pub(crate) fn flush(device: &wgpu::Device) {
    if !PENDING.load(Ordering::Acquire) {
        return;
    }
    BACKEND.state.lock().flush(device);
}

/// The region of texture `id` written since the last frame uploaded it.
#[cfg(test)]
pub(crate) fn pending_region(id: DynamicTextureId) -> Option<Bounds<DevicePixels>> {
    BACKEND.state.lock().textures.get(&id)?.dirty
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
    /// `None` while no context is published.
    texture: Option<wgpu::Texture>,
    size: Size<DevicePixels>,
    /// The texture's latest contents: tightly packed BGRA8 rows.
    contents: Vec<u8>,
    /// The region of `contents` the texture does not hold yet.
    dirty: Option<Bounds<DevicePixels>>,
}

impl UploadTexture {
    fn mark_dirty(&mut self, bounds: Bounds<DevicePixels>) {
        self.dirty = Some(match self.dirty {
            Some(dirty) => dirty.union(&bounds),
            None => bounds,
        });
        PENDING.store(true, Ordering::Release);
    }

    fn whole(&self) -> Bounds<DevicePixels> {
        Bounds {
            origin: point(DevicePixels(0), DevicePixels(0)),
            size: self.size,
        }
    }

    /// Copies tightly packed rows for `bounds` into `contents`.
    fn store(&mut self, bounds: Bounds<DevicePixels>, bytes: &[u8]) {
        let stride = self.size.width.0 as usize * 4;
        let row_bytes = bounds.size.width.0 as usize * 4;
        let x = bounds.origin.x.0 as usize * 4;
        for (row, source) in bytes
            .chunks_exact(row_bytes)
            .take(bounds.size.height.0 as usize)
            .enumerate()
        {
            let start = (bounds.origin.y.0 as usize + row) * stride + x;
            self.contents[start..start + row_bytes].copy_from_slice(source);
        }
        self.mark_dirty(bounds);
    }

    /// Writes the dirty region of `contents` into the texture.
    fn upload(&mut self, context: &WgpuContext) {
        let Some(texture) = &self.texture else {
            return;
        };
        let Some(dirty) = self.dirty.take() else {
            return;
        };
        let stride = self.size.width.0 as u32 * 4;
        let offset = (dirty.origin.y.0 as u64 * stride as u64) + dirty.origin.x.0 as u64 * 4;
        let extent = wgpu::Extent3d {
            width: dirty.size.width.0 as u32,
            height: dirty.size.height.0 as u32,
            depth_or_array_layers: 1,
        };
        let destination = wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d {
                x: dirty.origin.x.0 as u32,
                y: dirty.origin.y.0 as u32,
                z: 0,
            },
            aspect: wgpu::TextureAspect::All,
        };
        if texture.format() == wgpu::TextureFormat::Rgba8Unorm {
            // Atlas textures fall back to RGBA8 on adapters without BGRA8
            // sampling; swizzle only the rows being uploaded.
            let row_bytes = extent.width as usize * 4;
            let mut rows = Vec::with_capacity(row_bytes * extent.height as usize);
            for row in 0..extent.height as usize {
                let start = offset as usize + row * stride as usize;
                rows.extend_from_slice(&self.contents[start..start + row_bytes]);
            }
            let rows = crate::wgpu_atlas::swizzle_upload_data(&rows, texture.format());
            context.queue.write_texture(
                destination,
                &rows,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(extent.width * 4),
                    rows_per_image: Some(extent.height),
                },
                extent,
            );
        } else {
            context.queue.write_texture(
                destination,
                &self.contents,
                wgpu::TexelCopyBufferLayout {
                    offset,
                    bytes_per_row: Some(stride),
                    rows_per_image: Some(self.size.height.0 as u32),
                },
                extent,
            );
        }
    }
}

impl UploadState {
    fn use_context(&mut self, context: &WgpuContext) {
        if self
            .context
            .as_ref()
            .is_some_and(|current| current.device.as_ref() == context.device.as_ref())
        {
            return;
        }
        self.release();
        for (id, upload) in &mut self.textures {
            let texture = create_texture(context, upload.size);
            register(context, *id, &texture);
            upload.texture = Some(texture);
            let whole = upload.whole();
            upload.mark_dirty(whole);
        }
        self.context = Some(context.clone());
    }

    /// Drops the textures' GPU objects and the context, keeping the contents.
    fn release(&mut self) {
        let Some(context) = self.context.take() else {
            return;
        };
        let mut views = context.fast_dynamic_textures.0.lock();
        for (id, upload) in &mut self.textures {
            views.remove(id);
            upload.texture = None;
        }
    }

    fn context(&self) -> Result<&WgpuContext> {
        let context = self
            .context
            .as_ref()
            .context("no wgpu context is published")?;
        anyhow::ensure!(!context.device_lost(), "the GPU device is lost");
        Ok(context)
    }

    fn flush(&mut self, device: &wgpu::Device) {
        let Some(context) = self
            .context
            .as_ref()
            .filter(|context| context.device.as_ref() == device)
        else {
            return;
        };
        PENDING.store(false, Ordering::Release);
        for upload in self.textures.values_mut() {
            upload.upload(context);
        }
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
        register(context, id, &texture);
        let mut upload = UploadTexture {
            texture: Some(texture),
            size,
            contents: vec![0; size.width.0 as usize * size.height.0 as usize * 4],
            dirty: None,
        };
        let whole = upload.whole();
        upload.store(whole, bytes);
        state.textures.insert(id, upload);
        Ok(())
    }

    fn update_texture(
        &self,
        id: DynamicTextureId,
        bounds: Bounds<DevicePixels>,
        bytes: &[u8],
    ) -> Result<()> {
        // Stored while no context is published, too; the next device
        // receives it with the rest of the contents.
        let mut state = self.state.lock();
        state
            .textures
            .get_mut(&id)
            .context("unknown CPU dynamic texture")?
            .store(bounds, bytes);
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
