// Modified by Longbridge for gpui-fast.
//! `PaintSurface`s: CoreVideo images from screen capture or video decoding,
//! drawn as the Metal renderer drew them. Each biplanar full-range YCbCr
//! image is imported plane by plane through a CoreVideo Metal texture cache,
//! wrapped as wgpu textures, and drawn with upstream's `surfaces` pipeline,
//! which converts to RGB in `fs_surface`.
//!
//! A wrapped texture owns its `CVMetalTexture` through the hal drop callback.
//! wgpu destroys the hal texture only once no submission that uses it is
//! still running, so the CoreVideo texture, and the pixel buffer it retains,
//! outlive the GPU's reads without tracking submissions here.

use std::cell::OnceCell;
use std::fmt;
use std::ops::Range;
use std::ptr::{self, NonNull};
use std::sync::Once;

use gpui::{PaintSurface, Scene};
use objc2_core_foundation::CFRetained;
use objc2_core_video::{
    CVImageBuffer, CVMetalTexture, CVMetalTextureCache, CVMetalTextureGetTexture,
    CVPixelBufferGetHeightOfPlane, CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidthOfPlane,
    CVReturn, kCVPixelFormatType_420YpCbCr8BiPlanarFullRange, kCVReturnSuccess,
};
use objc2_metal::{MTLPixelFormat, MTLTextureType};
use wgpu::util::DeviceExt as _;

use crate::fast::frame::FrameTarget;
use crate::fast::pass_state::PassState;
use crate::wgpu_renderer::{SurfaceParams, WgpuRendererCore};

/// A renderer's CoreVideo texture cache, created on the first surface drawn.
#[derive(Default)]
pub(crate) struct PaintSurfaces {
    cache: OnceCell<Option<CFRetained<CVMetalTextureCache>>>,
}

/// What a surface's bind group is made from.
struct Inputs<'a> {
    device: &'a wgpu::Device,
    layout: &'a wgpu::BindGroupLayout,
    sampler: &'a wgpu::Sampler,
}

#[derive(Debug)]
enum SurfaceError {
    NotMetal,
    CreateCache { status: CVReturn },
    PixelFormat { format: u32 },
    EmptyPlane { plane: usize },
    CreateTexture { plane: usize, status: CVReturn },
    MissingTexture { plane: usize },
}

impl fmt::Display for SurfaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotMetal => write!(f, "the wgpu device is not a Metal device"),
            Self::CreateCache { status } => {
                write!(f, "creating CoreVideo Metal texture cache: status {status}")
            }
            Self::PixelFormat { format } => {
                write!(f, "unsupported surface pixel format: {format:#x}")
            }
            Self::EmptyPlane { plane } => write!(f, "surface plane {plane} is empty"),
            Self::CreateTexture { plane, status } => write!(
                f,
                "creating CoreVideo Metal texture for plane {plane}: status {status}"
            ),
            Self::MissingTexture { plane } => {
                write!(f, "retrieving Metal texture for plane {plane}")
            }
        }
    }
}

impl SurfaceError {
    /// Logs the error the first time one of its kind occurs in the process:
    /// a failing surface fails again on every frame.
    fn report(&self) {
        static NOT_METAL: Once = Once::new();
        static CREATE_CACHE: Once = Once::new();
        static PIXEL_FORMAT: Once = Once::new();
        static IMPORT: Once = Once::new();
        let once = match self {
            Self::NotMetal => &NOT_METAL,
            Self::CreateCache { .. } => &CREATE_CACHE,
            Self::PixelFormat { .. } => &PIXEL_FORMAT,
            Self::EmptyPlane { .. } | Self::CreateTexture { .. } | Self::MissingTexture { .. } => {
                &IMPORT
            }
        };
        once.call_once(|| log::error!("skipping paint surface: {self}"));
    }
}

/// Owns a CoreVideo texture from the hal drop callback of the wgpu texture
/// that wraps it.
struct KeepAlive {
    _texture: CFRetained<CVMetalTexture>,
}

// SAFETY: CoreVideo buffers are reference counted with thread-safe
// CFRetain/CFRelease. The value is never accessed, only dropped on the thread
// where wgpu destroys the texture.
unsafe impl Send for KeepAlive {}
// SAFETY: As above; no shared access exists.
unsafe impl Sync for KeepAlive {}

/// Forwarded to by the Surfaces arm of upstream's `record_frame`.
pub(crate) fn draw_core(
    core: &WgpuRendererCore,
    scene: &Scene,
    range: Range<usize>,
    pass: &mut wgpu::RenderPass<'_>,
) {
    let resources = core.resources();
    let inputs = Inputs {
        device: &resources.device,
        layout: &resources.bind_group_layouts.surfaces,
        sampler: &resources.atlas_sampler,
    };
    for surface in &scene.surfaces[range] {
        let Some(bind_group) = core.fast_frame.paint_surfaces.bind_group(&inputs, surface) else {
            continue;
        };
        pass.set_pipeline(&resources.pipelines.surfaces);
        crate::fast::rounded_clip::bind_unmasked(pass, resources);
        pass.set_bind_group(0, &resources.globals_bind_group, &[]);
        pass.set_bind_group(1, &bind_group, &[]);
        pass.draw(0..4, 0..1);
    }
}

/// Draws a Surfaces batch in `fast::frame::draw_scene`, whose step has bound
/// the rounded clip group.
pub(crate) fn draw(
    target: &FrameTarget,
    paint_surfaces: &PaintSurfaces,
    pass: &mut wgpu::RenderPass<'_>,
    bound: &mut PassState,
    globals: &wgpu::BindGroup,
    surfaces: &[PaintSurface],
) {
    let inputs = Inputs {
        device: target.device,
        layout: &target.bind_group_layouts.surfaces,
        sampler: target.atlas_sampler,
    };
    for surface in surfaces {
        let Some(bind_group) = paint_surfaces.bind_group(&inputs, surface) else {
            continue;
        };
        bound.set_pipeline(pass, &target.pipelines.surfaces);
        bound.set_bind_group(pass, 0, globals);
        bound.set_bind_group(pass, 1, &bind_group);
        pass.draw(0..4, 0..1);
    }
}

impl PaintSurfaces {
    /// The bind group drawing `surface`, or `None`, logged, when it cannot be imported.
    fn bind_group(&self, inputs: &Inputs, surface: &PaintSurface) -> Option<wgpu::BindGroup> {
        let cache = self.cache(inputs.device)?;
        surface_bind_group(inputs, cache, surface)
            .inspect_err(SurfaceError::report)
            .ok()
    }

    /// The cache, or `None` when it could not be created, which was logged then.
    fn cache(&self, device: &wgpu::Device) -> Option<&CVMetalTextureCache> {
        self.cache
            .get_or_init(|| create_cache(device).inspect_err(SurfaceError::report).ok())
            .as_deref()
    }
}

fn create_cache(device: &wgpu::Device) -> Result<CFRetained<CVMetalTextureCache>, SurfaceError> {
    // SAFETY: The hal device is only read for its MTLDevice, which the cache retains.
    let hal_device =
        unsafe { device.as_hal::<wgpu::hal::api::Metal>() }.ok_or(SurfaceError::NotMetal)?;
    let mut cache = ptr::null_mut();
    // SAFETY: The output pointer is writable and both optional attribute dictionaries are absent.
    let status = unsafe {
        CVMetalTextureCache::create(
            None,
            None,
            hal_device.raw_device(),
            None,
            NonNull::from(&mut cache),
        )
    };
    if status != kCVReturnSuccess {
        return Err(SurfaceError::CreateCache { status });
    }
    let cache = NonNull::new(cache).ok_or(SurfaceError::CreateCache { status })?;
    // SAFETY: The successful create call transfers ownership of this non-null cache.
    Ok(unsafe { CFRetained::from_raw(cache) })
}

fn surface_bind_group(
    inputs: &Inputs,
    cache: &CVMetalTextureCache,
    surface: &PaintSurface,
) -> Result<wgpu::BindGroup, SurfaceError> {
    let image = &surface.image_buffer;
    let format = CVPixelBufferGetPixelFormatType(image);
    if format != kCVPixelFormatType_420YpCbCr8BiPlanarFullRange {
        return Err(SurfaceError::PixelFormat { format });
    }
    let y = import_plane(
        inputs.device,
        cache,
        image,
        0,
        MTLPixelFormat::R8Unorm,
        wgpu::TextureFormat::R8Unorm,
    )?;
    let cb_cr = import_plane(
        inputs.device,
        cache,
        image,
        1,
        MTLPixelFormat::RG8Unorm,
        wgpu::TextureFormat::Rg8Unorm,
    )?;
    let params = SurfaceParams {
        bounds: surface.bounds.into(),
        content_mask: surface.content_mask.bounds.into(),
    };
    let params = inputs
        .device
        .create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("paint_surface_params"),
            contents: bytemuck::bytes_of(&params),
            usage: wgpu::BufferUsages::UNIFORM,
        });
    Ok(inputs.device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("paint_surface_bind_group"),
        layout: inputs.layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: params.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::TextureView(&y),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: wgpu::BindingResource::TextureView(&cb_cr),
            },
            wgpu::BindGroupEntry {
                binding: 3,
                resource: wgpu::BindingResource::Sampler(inputs.sampler),
            },
        ],
    }))
}

/// A view of plane `plane` of `image`, imported without a copy.
fn import_plane(
    device: &wgpu::Device,
    cache: &CVMetalTextureCache,
    image: &CVImageBuffer,
    plane: usize,
    metal_format: MTLPixelFormat,
    format: wgpu::TextureFormat,
) -> Result<wgpu::TextureView, SurfaceError> {
    let width = CVPixelBufferGetWidthOfPlane(image, plane);
    let height = CVPixelBufferGetHeightOfPlane(image, plane);
    if width == 0 || height == 0 {
        return Err(SurfaceError::EmptyPlane { plane });
    }
    let mut texture = ptr::null_mut();
    // SAFETY: The buffer is biplanar 4:2:0, so `plane` exists with these dimensions and
    // a format of this size per texel, and the output pointer is writable.
    let status = unsafe {
        CVMetalTextureCache::create_texture_from_image(
            None,
            cache,
            image,
            None,
            metal_format,
            width,
            height,
            plane,
            NonNull::from(&mut texture),
        )
    };
    if status != kCVReturnSuccess {
        return Err(SurfaceError::CreateTexture { plane, status });
    }
    let texture = NonNull::new(texture).ok_or(SurfaceError::CreateTexture { plane, status })?;
    // SAFETY: The successful create call transfers ownership of this non-null texture.
    let texture = unsafe { CFRetained::from_raw(texture) };
    let raw = CVMetalTextureGetTexture(&texture).ok_or(SurfaceError::MissingTexture { plane })?;

    let (width, height) = (width as u32, height as u32);
    let keep_alive = KeepAlive { _texture: texture };
    // SAFETY: `raw` is a 2D texture of `format`'s layout with one layer and one mip level,
    // and the callback keeps the CoreVideo texture backing it alive as long as wgpu uses it.
    let hal_texture = unsafe {
        wgpu::hal::metal::Device::texture_from_raw(
            raw,
            format,
            MTLTextureType::Type2D,
            1,
            1,
            wgpu::hal::CopyExtent {
                width,
                height,
                depth: 1,
            },
            Some(Box::new(move || drop(keep_alive))),
        )
    };
    // SAFETY: The hal texture comes from this device's MTLDevice through the cache, matches the
    // descriptor, and holds the decoded image, which shaders may read without a transition.
    let texture = unsafe {
        device.create_texture_from_hal::<wgpu::hal::api::Metal>(
            hal_texture,
            &wgpu::TextureDescriptor {
                label: Some("paint_surface_plane"),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            },
            wgpu::TextureUses::RESOURCE,
            true,
        )
    };
    Ok(texture.create_view(&wgpu::TextureViewDescriptor::default()))
}
