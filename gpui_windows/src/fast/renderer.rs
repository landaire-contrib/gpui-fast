// Modified by Longbridge for gpui-fast.
//! Windows draw through `gpui_wgpu` on Direct3D 12.
//!
//! A window keeps upstream's DirectComposition tree: a target for the window,
//! created with `WS_EX_NOREDIRECTIONBITMAP`, and a visual at its root that
//! wgpu presents into (`SurfaceTargetUnsafe::CompositionVisual`), so
//! transparent backgrounds, Mica and window composition work as they did with
//! upstream's Direct3D 11 renderer. With `GPUI_DISABLE_DIRECT_COMPOSITION`
//! set, wgpu presents to the window's handle instead.
//!
//! DirectWrite still rasterizes glyphs, color glyphs on the platform's
//! Direct3D 11 device (`direct_write.rs`); the bytes go into the renderer's
//! atlas, and DirectWrite's gamma and contrast go into its text shaders.
//!
//! The platform's device loss detection and recovery are upstream's, driven by
//! that Direct3D 11 device; [`take_device_loss`] adds the loss of the wgpu
//! device to what the VSync thread checks each time it wakes.

use std::{borrow::Cow, cell::RefCell, num::NonZeroIsize, rc::Rc, sync::Arc};

use anyhow::{Context as _, Result};
use gpui::{
    AtlasKey, AtlasTextureKind, AtlasTile, DevicePixels, GpuSpecs, PlatformAtlas, Scene, Size,
    WindowBackgroundAppearance, size,
};
use gpui_util::ResultExt;
use gpui_wgpu::{
    CompositorGpuHint, DeviceErrorState, GpuContext, WgpuAtlas, WgpuRenderer, WgpuSurfaceConfig,
    WgpuSurfaceSource, WgpuTextRenderingParams, wgpu,
};
use parking_lot::Mutex;
use raw_window_handle as rwh;
use windows::{
    Win32::{
        Foundation::HWND,
        Graphics::{
            Direct3D11::ID3D11Device,
            DirectComposition::{
                DCompositionCreateDevice, IDCompositionDevice, IDCompositionTarget,
                IDCompositionVisual,
            },
            Dxgi::{IDXGIAdapter1, IDXGIDevice, IDXGIFactory6},
        },
    },
    core::Interface,
};

use crate::{DirectXDevices, try_to_recover_from_device_lost};

/// The device errors of the wgpu context windows draw with, for the VSync
/// thread to notice its loss. Taken when the loss is reported, and set again
/// by the window that recovers.
static WATCHED_DEVICE: Mutex<Option<Arc<DeviceErrorState>>> = Mutex::new(None);

thread_local! {
    /// The wgpu context every window of the UI thread draws with.
    static SHARED_GPU: SharedGpu = SharedGpu::default();
}

#[derive(Default)]
struct SharedGpu {
    context: GpuContext,
    /// The platform's Direct3D 11 device when the context was made. The
    /// platform recreates that device to recover from any device loss, so a
    /// window that recovers with another one recreates the context too, even
    /// if wgpu has not yet seen its device fail.
    platform_device: RefCell<Option<ID3D11Device>>,
}

/// A window's renderer.
pub(crate) struct WindowsRenderer {
    hwnd: HWND,
    /// Draws the window's base surface: the visual at the root of `composition`,
    /// or the window itself without DirectComposition.
    pub(crate) gpu: WgpuRenderer,
    /// Whether the window composes through DirectComposition.
    composited: bool,
    /// `None` when DirectComposition is disabled, and while it is rebuilt.
    pub(crate) composition: Option<DirectComposition>,
    atlas: Arc<WindowsAtlas>,
    pub(crate) size: Size<DevicePixels>,
    /// The platform's DXGI factory, to find the adapter wgpu draws with.
    dxgi_factory: IDXGIFactory6,

    /// Set by a device recovery until the window's next forced render: the
    /// atlas was cleared, and the scene drawn before it still names its tiles.
    skip_draws: bool,
    /// Whether a surface may hold a swap chain DirectComposition does not show
    /// yet. wgpu makes a new one each time it configures a surface, which it
    /// gives the visual without committing; committing once the next frame is
    /// presented keeps the last one on screen until then.
    pub(crate) commit_pending: bool,
    pub(crate) fast_composition: crate::fast::composition::WindowComposition,
    pub(crate) fast_recovery: crate::fast::device_recovery::RecoveryState,
}

/// Upstream's DirectComposition objects of a window: its target and the
/// visual at its root.
pub(crate) struct DirectComposition {
    pub(crate) comp_device: IDCompositionDevice,
    pub(crate) comp_target: IDCompositionTarget,
    pub(crate) comp_visual: IDCompositionVisual,
}

/// What a window's wgpu surfaces present into.
pub(crate) enum SurfaceTarget {
    Visual(IDCompositionVisual),
    Window(HWND),
}

/// The window's sprite atlas: `gpui_wgpu`'s, which stores color tiles in BGRA
/// order, with DirectWrite's subpixel glyphs, which come in RGB order,
/// reordered on the way in.
struct WindowsAtlas(Arc<WgpuAtlas>);

impl WindowsRenderer {
    pub(crate) fn new(
        hwnd: HWND,
        directx_devices: &DirectXDevices,
        disable_direct_composition: bool,
    ) -> Result<Self> {
        if disable_direct_composition {
            log::info!("Direct Composition is disabled.");
        }

        let composition = if disable_direct_composition {
            None
        } else {
            Some(
                DirectComposition::new(directx_devices, hwnd)
                    .context("Creating DirectComposition")?,
            )
        };
        let target = SurfaceTarget::new(composition.as_ref(), hwnd);
        let gpu_context = SHARED_GPU.with(|shared| shared.context_for(directx_devices));
        let config = surface_config(
            size(DevicePixels(1), DevicePixels(1)),
            composition.is_some(),
        );
        let mut gpu = WgpuRenderer::new_from_source(
            Rc::clone(&gpu_context),
            &target,
            config,
            compositor_gpu(&directx_devices.adapter),
        )
        .context("Creating wgpu renderer")?;
        configure_text(&mut gpu);
        watch_device(&gpu_context);
        if let Some(composition) = &composition {
            composition.commit()?;
        }
        let atlas = Arc::new(WindowsAtlas(Arc::clone(gpu.sprite_atlas())));
        Ok(Self {
            hwnd,
            gpu,
            composited: composition.is_some(),
            composition,
            atlas,
            size: size(DevicePixels(1), DevicePixels(1)),
            dxgi_factory: directx_devices.dxgi_factory.clone(),
            skip_draws: false,
            commit_pending: false,
            fast_composition: crate::fast::composition::WindowComposition::default(),
            fast_recovery: crate::fast::device_recovery::RecoveryState::default(),
        })
    }

    pub(crate) fn sprite_atlas(&self) -> Arc<dyn PlatformAtlas> {
        self.atlas.clone()
    }

    pub(crate) fn draw(
        &mut self,
        scene: &Scene,
        background_appearance: WindowBackgroundAppearance,
    ) -> Result<()> {
        if !self.can_draw() || !crate::fast::device_recovery::prepare_frame(self) {
            return Ok(());
        }
        let presented = self.draw_base(scene, background_appearance);
        crate::fast::device_recovery::finish_frame(self, presented, presented);
        Ok(())
    }

    /// Draws `scene` onto the base surface and presents it, returning whether
    /// it was presented.
    pub(crate) fn draw_base(
        &mut self,
        scene: &Scene,
        background_appearance: WindowBackgroundAppearance,
    ) -> bool {
        self.gpu.set_clear_color(clear_color(background_appearance));
        let presented = self.gpu.draw(scene);
        #[cfg(test)]
        crate::fast::native_tests::present(self.hwnd, self.composited, !presented as i32);
        presented
    }

    /// Shows the swap chains wgpu made since the last commit, once a frame
    /// has been presented into them.
    pub(crate) fn commit_if_pending(&mut self) {
        if !std::mem::take(&mut self.commit_pending) {
            return;
        }
        if let Some(composition) = &self.composition {
            composition.commit().log_err();
        }
    }

    /// Whether frames can be drawn: not while a lost device awaits recovery,
    /// nor before the forced render that follows one.
    pub(crate) fn can_draw(&self) -> bool {
        !self.skip_draws && !self.gpu.device_lost()
    }

    #[cfg(feature = "test-support")]
    pub(crate) fn render_to_image(
        &mut self,
        scene: &Scene,
        background_appearance: WindowBackgroundAppearance,
    ) -> Result<image::RgbaImage> {
        anyhow::ensure!(
            self.can_draw(),
            "render_to_image unavailable while recovering from a lost device"
        );
        self.gpu.set_clear_color(clear_color(background_appearance));
        self.gpu.render_to_image(scene)
    }

    /// Resizes the window's surfaces, returning whether the size changed.
    pub(crate) fn resize(&mut self, new_size: Size<DevicePixels>) -> bool {
        let new_size = size(
            DevicePixels(new_size.width.0.max(1)),
            DevicePixels(new_size.height.0.max(1)),
        );
        if self.size == new_size {
            return false;
        }

        self.size = new_size;
        self.gpu.update_drawable_size(new_size);
        crate::fast::composition::resize(self);
        self.commit_pending = true;
        true
    }

    /// Rebuilds the window's DirectComposition tree on the platform's new
    /// devices, and its wgpu renderers on a recovered context.
    pub(crate) fn handle_device_lost(&mut self, directx_devices: &DirectXDevices) -> Result<()> {
        try_to_recover_from_device_lost(|| {
            self.recover(directx_devices)
                .context("WindowsRenderer handling device lost")
        })
    }

    fn recover(&mut self, directx_devices: &DirectXDevices) -> Result<()> {
        crate::fast::composition::release(self);
        // Release every reference to the old DirectComposition device's
        // objects, so that its target leaves the window: a window holds one
        // DirectComposition target at a time.
        self.gpu.unconfigure_surface();
        self.composition = None;

        let composition = if self.composited {
            Some(DirectComposition::new(directx_devices, self.hwnd)?)
        } else {
            None
        };
        let target = SurfaceTarget::new(composition.as_ref(), self.hwnd);
        let gpu_context = SHARED_GPU.with(|shared| shared.context_for(directx_devices));
        self.gpu.recover_from_source(&target)?;
        configure_text(&mut self.gpu);
        watch_device(&gpu_context);
        self.composition = composition;
        self.dxgi_factory = directx_devices.dxgi_factory.clone();
        crate::fast::composition::recreate(self)?;
        if let Some(composition) = &self.composition {
            composition.commit()?;
        }
        self.skip_draws = true;
        Ok(())
    }

    /// Upstream's description of the DXGI adapter wgpu draws with, or wgpu's
    /// when DXGI does not list it.
    pub(crate) fn gpu_specs(&self) -> Result<GpuSpecs> {
        let wgpu_specs = self
            .gpu
            .gpu_specs()
            .context("GPU resources are released until the device recovers")?;
        let info = SHARED_GPU.with(|shared| {
            shared
                .context
                .borrow()
                .as_ref()
                .map(|context| context.adapter.get_info())
        });
        let Some(adapter) = info.and_then(|info| dxgi_adapter(&self.dxgi_factory, &info)) else {
            return Ok(wgpu_specs);
        };

        crate::directx_renderer::gpu_specs(&adapter)
    }

    pub(crate) fn mark_drawable(&mut self) {
        self.skip_draws = false;
    }

    /// Whether `gpui_wgpu` cleared the atlas after repeated GPU errors, which
    /// the next frame must draw everything again for.
    pub(crate) fn take_needs_redraw(&mut self) -> bool {
        let overlays = crate::fast::composition::take_needs_redraw(self);
        self.gpu.needs_redraw() || overlays
    }
}

impl DirectComposition {
    fn new(directx_devices: &DirectXDevices, hwnd: HWND) -> Result<Self> {
        let dxgi_device: IDXGIDevice = directx_devices
            .device
            .cast()
            .context("Creating DXGI device")?;
        let comp_device: IDCompositionDevice = unsafe { DCompositionCreateDevice(&dxgi_device)? };
        let comp_target = unsafe { comp_device.CreateTargetForHwnd(hwnd, true) }?;
        let comp_visual = unsafe { comp_device.CreateVisual() }?;
        unsafe { comp_target.SetRoot(&comp_visual) }?;
        Ok(Self {
            comp_device,
            comp_target,
            comp_visual,
        })
    }

    /// Shows what wgpu set: it gives a visual its swap chain without committing.
    pub(crate) fn commit(&self) -> Result<()> {
        unsafe { self.comp_device.Commit() }.context("Committing DirectComposition")
    }
}

impl SurfaceTarget {
    fn new(composition: Option<&DirectComposition>, hwnd: HWND) -> Self {
        match composition {
            Some(composition) => Self::Visual(composition.comp_visual.clone()),
            None => Self::Window(hwnd),
        }
    }
}

impl WgpuSurfaceSource for SurfaceTarget {
    fn create_surface(&self, instance: &wgpu::Instance) -> Result<wgpu::Surface<'static>> {
        let target = match self {
            // wgpu holds a reference to the visual for as long as the surface lives.
            Self::Visual(visual) => wgpu::SurfaceTargetUnsafe::CompositionVisual(visual.as_raw()),
            Self::Window(hwnd) => {
                let handle = NonZeroIsize::new(hwnd.0 as isize).context("null window handle")?;
                wgpu::SurfaceTargetUnsafe::RawHandle {
                    raw_display_handle: Some(rwh::RawDisplayHandle::Windows(
                        rwh::WindowsDisplayHandle::new(),
                    )),
                    raw_window_handle: rwh::RawWindowHandle::Win32(rwh::Win32WindowHandle::new(
                        handle,
                    )),
                }
            }
        };
        // SAFETY: a visual is reference counted by the surface; a window outlives
        // its renderer, which owns every surface made for it.
        unsafe { instance.create_surface_unsafe(target) }.context("Creating wgpu surface")
    }
}

impl PlatformAtlas for WindowsAtlas {
    fn get_or_insert_with<'a>(
        &self,
        key: AtlasKey,
        build: &mut dyn FnMut() -> Result<Option<(Size<DevicePixels>, Cow<'a, [u8]>)>>,
    ) -> Result<Option<AtlasTile>> {
        if key.texture_kind() != AtlasTextureKind::Subpixel {
            return self.0.get_or_insert_with(key, build);
        }
        self.0.get_or_insert_with(key, &mut || {
            Ok(build()?.map(|(size, bytes)| {
                let mut bytes = bytes.into_owned();
                for pixel in bytes.chunks_exact_mut(4) {
                    pixel.swap(0, 2);
                }
                (size, Cow::Owned(bytes))
            }))
        })
    }

    fn remove(&self, key: &AtlasKey) {
        self.0.remove(key);
    }

    #[cfg(feature = "test-support")]
    fn contains(&self, key: &AtlasKey) -> bool {
        self.0.contains(key)
    }
}

impl SharedGpu {
    /// The shared context for a window made or recovered on the platform's
    /// `directx_devices`, emptied when it must be made again.
    fn context_for(&self, directx_devices: &DirectXDevices) -> GpuContext {
        let mut platform_device = self.platform_device.borrow_mut();
        let lost = self
            .context
            .borrow()
            .as_ref()
            .is_some_and(|context| context.device_lost());
        if lost || platform_device.as_ref() != Some(&directx_devices.device) {
            self.context.borrow_mut().take();
            *platform_device = Some(directx_devices.device.clone());
        }
        Rc::clone(&self.context)
    }
}

/// Forwarded to by `WindowsPlatform::detach_gpu`: releases the context once
/// no window draws with it.
pub(crate) fn release_gpu() {
    SHARED_GPU.with(|shared| {
        shared.context.borrow_mut().take();
        shared.platform_device.borrow_mut().take();
    });
    WATCHED_DEVICE.lock().take();
}

/// Forwarded to by the VSync thread with its own device loss check: whether
/// the wgpu device windows draw with was lost. Reported once per device.
pub(crate) fn take_device_loss() -> bool {
    let mut watched = WATCHED_DEVICE.lock();
    let lost = watched.as_ref().is_some_and(|errors| errors.device_lost());
    if lost {
        log::error!("wgpu device lost detected");
        watched.take();
    }
    lost
}

/// Forwarded to by `WindowsWindow::draw` once it has drawn: has the next
/// frame draw everything again if the renderer needs it, and asks for it.
pub(crate) fn after_draw(window: &crate::WindowsWindow) {
    if window.state.renderer.borrow_mut().take_needs_redraw() {
        window.state.force_render_pending.set(true);
    }
    crate::fast::frame_schedule::schedule_frame(window);
}

/// Has wgpu prefer the adapter of the platform's Direct3D 11 device, which
/// upstream draws with: the first DXGI adapter that supports Direct3D 11.
fn compositor_gpu(adapter: &IDXGIAdapter1) -> Option<CompositorGpuHint> {
    let desc = unsafe { adapter.GetDesc1() }
        .context("Reading the platform's DXGI adapter")
        .log_err()?;
    Some(CompositorGpuHint {
        vendor_id: desc.VendorId,
        device_id: desc.DeviceId,
    })
}

/// The adapter `factory` lists for the one wgpu describes as `info`.
fn dxgi_adapter(factory: &IDXGIFactory6, info: &wgpu::AdapterInfo) -> Option<IDXGIAdapter1> {
    (0..)
        .map_while(|index| unsafe { factory.EnumAdapters1(index) }.ok())
        .find(|adapter| {
            unsafe { adapter.GetDesc1() }
                .is_ok_and(|desc| desc.VendorId == info.vendor && desc.DeviceId == info.device)
        })
}

fn watch_device(gpu_context: &GpuContext) {
    let errors = gpu_context
        .borrow()
        .as_ref()
        .map(|context| Arc::clone(context.errors()));
    *WATCHED_DEVICE.lock() = errors;
}

/// The configuration of a window's surfaces. As upstream's swap chains, they
/// are premultiplied whenever DirectComposition blends them with what lies
/// under the window, whatever the background; an opaque background is drawn
/// opaque instead (see `clear_color`), so changing it reconfigures nothing.
pub(crate) fn surface_config(size: Size<DevicePixels>, composited: bool) -> WgpuSurfaceConfig {
    WgpuSurfaceConfig {
        size,
        transparent: composited,
        // Presents without waiting for vertical blank, as upstream's
        // `Present(0, 0)`: the VSync thread paces frames.
        preferred_present_mode: Some(wgpu::PresentMode::Mailbox),
    }
}

/// Applies DirectWrite's rendering parameters, which upstream's shaders read
/// from the same place, to the renderer's text shaders.
pub(crate) fn configure_text(gpu: &mut WgpuRenderer) {
    let font_info = WindowsRenderer::get_font_info();
    gpu.set_subpixel_layout(font_info.is_bgr);
    gpu.set_text_rendering_params(WgpuTextRenderingParams {
        gamma_ratios: font_info.gamma_ratios,
        grayscale_enhanced_contrast: font_info.grayscale_enhanced_contrast,
        subpixel_enhanced_contrast: font_info.subpixel_enhanced_contrast,
    });
}

/// What a base surface is cleared to before drawing. Upstream clears an opaque
/// window to white: under DirectComposition its swap chain blends with the
/// desktop.
fn clear_color(background_appearance: WindowBackgroundAppearance) -> wgpu::Color {
    match background_appearance {
        WindowBackgroundAppearance::Opaque => wgpu::Color::WHITE,
        WindowBackgroundAppearance::Transparent
        | WindowBackgroundAppearance::Blurred
        | WindowBackgroundAppearance::MicaBackdrop
        | WindowBackgroundAppearance::MicaAltBackdrop => wgpu::Color::TRANSPARENT,
    }
}
