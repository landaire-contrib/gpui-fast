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
//! Where Direct3D 12 cannot draw (no adapter, as on a machine whose only
//! Direct3D 12 driver wgpu refuses, or a failed device), windows open without
//! DirectComposition and present into their handle through Vulkan, or OpenGL
//! when Vulkan fails too. Such windows are opaque: transparent, blurred and
//! Mica backgrounds show black, and window composition is unavailable. WARP,
//! the software adapter, counts as Direct3D 12, so a machine without a usable
//! hardware adapter (Intel Haswell graphics, which wgpu excludes from Direct3D
//! 12) keeps composition and draws on the CPU.
//!
//! DirectWrite still rasterizes glyphs, color glyphs on the platform's
//! Direct3D 11 device (`direct_write.rs`); the bytes go into the renderer's
//! atlas, and DirectWrite's gamma and contrast go into its text shaders.
//!
//! The platform's device loss detection and recovery are upstream's, driven by
//! that Direct3D 11 device; [`take_device_loss`] adds the loss of the wgpu
//! device to what the VSync thread checks each time it wakes.

use std::{
    borrow::Cow,
    cell::{Cell, RefCell},
    num::NonZeroIsize,
    rc::Rc,
    sync::Arc,
};

use anyhow::{Context as _, Result};
use gpui::{
    AtlasKey, AtlasTextureKind, AtlasTile, DevicePixels, GpuSpecs, PlatformAtlas, Scene, Size,
    WindowBackgroundAppearance, size,
};
use gpui_util::ResultExt;
use gpui_wgpu::{
    CompositorGpuHint, DeviceErrorState, GpuContext, WgpuAtlas, WgpuContext, WgpuRenderer,
    WgpuSurfaceConfig, WgpuSurfaceSource, WgpuTextRenderingParams, wgpu,
};
use parking_lot::Mutex;
use raw_window_handle as rwh;
use smallvec::{SmallVec, smallvec};
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

use crate::{DirectXDevices, WindowsWindowInner, try_to_recover_from_device_lost};

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
    /// Whether Direct3D 12 can draw windows: probed when the first window
    /// opens, and false from when a window failed to draw with it.
    dx12: Cell<Option<bool>>,
    /// Set when Direct3D 12 failed to draw the first window into
    /// DirectComposition, for later windows to open without it.
    composition_failed: Cell<bool>,
    /// The backend the first window drew with. Every window draws with it,
    /// also while the shared context is dropped for a device recovery.
    backend: Cell<Option<wgpu::Backends>>,
}

/// A window's renderer.
pub(crate) struct WindowsRenderer {
    hwnd: HWND,
    /// Draws the window's base surface: the visual at the root of `composition`,
    /// or the window itself without DirectComposition.
    pub(crate) gpu: WgpuRenderer,
    /// Whether the window composes through DirectComposition.
    composited: bool,
    /// The backend the window's surface presents through.
    backends: wgpu::Backends,
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
    /// yet. Each configure of a wgpu surface makes a new swap chain (wgpu-core
    /// unconfigures the surface first, which releases the old one), and wgpu
    /// gives it to the visual without committing; committing once the next
    /// frame is presented keeps the last one on screen until then.
    pub(crate) commit_pending: bool,
    /// Whether a frame was not presented on every surface, and the next one
    /// must present the scene again even if nothing changed.
    pub(crate) present_pending: bool,
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
    /// A DirectComposition visual, through Direct3D 12.
    Visual(IDCompositionVisual),
    /// The window itself, through `backends`.
    Window {
        hwnd: HWND,
        backends: wgpu::Backends,
    },
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

        let gpu_context = SHARED_GPU.with(|shared| shared.context_for(directx_devices));
        let compositor_gpu = compositor_gpu(&directx_devices.adapter);
        let mut composition = if disable_direct_composition {
            None
        } else {
            Some(
                DirectComposition::new(directx_devices, hwnd)
                    .context("Creating DirectComposition")?,
            )
        };
        let composited = match &composition {
            Some(composition) => WgpuRenderer::new_from_source(
                Rc::clone(&gpu_context),
                &SurfaceTarget::Visual(composition.comp_visual.clone()),
                surface_config(size(DevicePixels(1), DevicePixels(1)), true),
                compositor_gpu,
            )
            .inspect_err(|error| {
                SHARED_GPU.with(|shared| {
                    if shared.backend.get().is_none() {
                        shared.composition_failed.set(true);
                    }
                });
                log::error!(
                    "Cannot draw the window into DirectComposition, drawing into the window \
                     without it: {error:#}"
                );
            })
            .ok(),
            None => None,
        };
        let (mut gpu, backends) = match composited {
            Some(gpu) => (gpu, wgpu::Backends::DX12),
            None => {
                // The window's DirectComposition target would hide what wgpu
                // presents into the window. The window keeps the
                // `WS_EX_NOREDIRECTIONBITMAP` it was created with, under
                // which Vulkan or OpenGL presentation may not show; later
                // windows open without it (`disable_direct_composition`).
                composition = None;
                new_window_renderer(&gpu_context, hwnd, compositor_gpu)?
            }
        };
        configure_text(&mut gpu);
        watch_device(&gpu_context);
        SHARED_GPU.with(|shared| shared.backend.set(Some(backends)));
        if let Some(composition) = &composition {
            composition.commit()?;
        }
        let atlas = Arc::new(WindowsAtlas(Arc::clone(gpu.sprite_atlas())));
        Ok(Self {
            hwnd,
            gpu,
            composited: composition.is_some(),
            backends,
            composition,
            atlas,
            size: size(DevicePixels(1), DevicePixels(1)),
            dxgi_factory: directx_devices.dxgi_factory.clone(),
            skip_draws: false,
            commit_pending: false,
            present_pending: false,
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
        let target = match &composition {
            Some(composition) => SurfaceTarget::Visual(composition.comp_visual.clone()),
            None => SurfaceTarget::Window {
                hwnd: self.hwnd,
                backends: self.backends,
            },
        };
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

impl WgpuSurfaceSource for SurfaceTarget {
    fn create_surface(&self, instance: &wgpu::Instance) -> Result<wgpu::Surface<'static>> {
        let target = match self {
            // wgpu holds a reference to the visual for as long as the surface lives.
            Self::Visual(visual) => wgpu::SurfaceTargetUnsafe::CompositionVisual(visual.as_raw()),
            Self::Window { hwnd, .. } => {
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

    fn backends(&self) -> wgpu::Backends {
        match self {
            Self::Visual(_) => wgpu::Backends::DX12,
            Self::Window { backends, .. } => *backends,
        }
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
                reorder_subpixel_coverage(&mut bytes);
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

    /// Whether Direct3D 12 can draw windows, probed the first time it is asked.
    fn dx12_usable(&self) -> bool {
        if let Some(usable) = self.dx12.get() {
            return usable;
        }
        let usable = probe_dx12();
        self.dx12.set(Some(usable));
        usable
    }

    /// The backends to try, in order, for a window presenting into its handle:
    /// the first window's, once one has drawn, since every window draws with
    /// the shared device.
    fn window_backends(&self) -> SmallVec<[wgpu::Backends; 3]> {
        if let Some(backend) = self.backend.get() {
            return smallvec![backend];
        }
        if self.dx12_usable() {
            smallvec![
                wgpu::Backends::DX12,
                wgpu::Backends::VULKAN,
                wgpu::Backends::GL
            ]
        } else {
            smallvec![wgpu::Backends::VULKAN, wgpu::Backends::GL]
        }
    }
}

/// Forwarded to by `WindowsPlatform::generate_creation_info`: whether a new
/// window opens without DirectComposition, because it was asked to, because
/// Direct3D 12 cannot draw windows, or because it could not draw an earlier
/// window into DirectComposition.
pub(crate) fn disable_direct_composition(requested: bool) -> bool {
    requested || SHARED_GPU.with(|shared| shared.composition_failed.get() || !shared.dx12_usable())
}

/// Forwarded to by `WindowsWindowInner::draw_window`: whether the frame must
/// present the scene again, because the last one was not presented.
pub(crate) fn take_present_pending(window: &WindowsWindowInner) -> bool {
    std::mem::take(&mut window.state.renderer.borrow_mut().present_pending)
}

/// Creates the renderer of a window presenting into its handle, trying each of
/// [`SharedGpu::window_backends`] in turn. A first window that cannot draw
/// with Direct3D 12 marks it unusable, so later windows open without
/// DirectComposition.
fn new_window_renderer(
    gpu_context: &GpuContext,
    hwnd: HWND,
    compositor_gpu: Option<CompositorGpuHint>,
) -> Result<(WgpuRenderer, wgpu::Backends)> {
    let first_window = SHARED_GPU.with(|shared| shared.backend.get().is_none());
    let mut last_error = None;
    for backends in SHARED_GPU.with(SharedGpu::window_backends) {
        let target = SurfaceTarget::Window { hwnd, backends };
        let config = surface_config(size(DevicePixels(1), DevicePixels(1)), false);
        match WgpuRenderer::new_from_source(Rc::clone(gpu_context), &target, config, compositor_gpu)
        {
            Ok(gpu) => return Ok((gpu, backends)),
            Err(error) => {
                log::error!("Cannot draw the window with {backends:?}: {error:#}");
                if backends == wgpu::Backends::DX12 && first_window {
                    SHARED_GPU.with(|shared| shared.dx12.set(Some(false)));
                }
                last_error = Some(error);
            }
        }
    }
    Err(last_error
        .unwrap_or_else(|| anyhow::anyhow!("no wgpu backend to try"))
        .context("Creating wgpu renderer"))
}

/// Whether wgpu creates a Direct3D 12 device on some adapter, WARP included.
/// Decided before the first window is created, since a window created for
/// DirectComposition cannot draw through Vulkan or OpenGL reliably.
fn probe_dx12() -> bool {
    let instance = WgpuContext::instance_with_backends(wgpu::Backends::DX12, None);
    let usable = gpui::block_on(async {
        for adapter in instance.enumerate_adapters(wgpu::Backends::DX12).await {
            let info = adapter.get_info();
            match adapter
                .request_device(&wgpu::DeviceDescriptor::default())
                .await
            {
                Ok(_) => {
                    log::info!(
                        "Direct3D 12 adapter: {} ({:?})",
                        info.name,
                        info.device_type
                    );
                    return true;
                }
                Err(error) => {
                    log::warn!("Direct3D 12 adapter {} has no device: {error}", info.name);
                }
            }
        }
        false
    });
    if !usable {
        log::warn!(
            "Direct3D 12 cannot draw: windows draw with Vulkan or OpenGL, opaque and without \
             window composition"
        );
    }
    usable
}

/// DirectWrite gives ClearType coverage in red, green, blue order, with an
/// unused fourth byte; `WgpuAtlas` takes color tiles in blue, green, red
/// order and reorders them itself for an RGBA texture.
fn reorder_subpixel_coverage(bytes: &mut [u8]) {
    for pixel in bytes.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
}

/// Forwarded to by `WindowsPlatform::detach_gpu`: releases the context once
/// no window draws with it, including the copy published to applications, and
/// forgets which backends worked, since the GPU may differ when the platform
/// becomes windowed again.
pub(crate) fn release_gpu() {
    // Applications release their objects on the device before GPUI does.
    gpui_wgpu::release_app_context();
    SHARED_GPU.with(|shared| {
        shared.context.borrow_mut().take();
        shared.platform_device.borrow_mut().take();
        shared.dx12.set(None);
        shared.composition_failed.set(false);
        shared.backend.set(None);
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

#[cfg(test)]
mod tests {
    use gpui_wgpu::wgpu;

    use super::{SharedGpu, reorder_subpixel_coverage};

    #[test]
    fn subpixel_coverage_is_reordered_from_rgb_to_bgr() {
        let mut bytes = [10, 20, 30, 0, 40, 50, 60, 0];
        reorder_subpixel_coverage(&mut bytes);
        assert_eq!(bytes, [30, 20, 10, 0, 60, 50, 40, 0]);
    }

    #[test]
    fn windows_fall_back_to_vulkan_then_gl_without_direct3d_12() {
        let shared = SharedGpu::default();
        shared.dx12.set(Some(true));
        assert_eq!(
            shared.window_backends().as_slice(),
            [
                wgpu::Backends::DX12,
                wgpu::Backends::VULKAN,
                wgpu::Backends::GL
            ]
        );
        shared.dx12.set(Some(false));
        assert_eq!(
            shared.window_backends().as_slice(),
            [wgpu::Backends::VULKAN, wgpu::Backends::GL]
        );
        // Once a window has drawn, every window uses its backend.
        shared.backend.set(Some(wgpu::Backends::VULKAN));
        assert_eq!(
            shared.window_backends().as_slice(),
            [wgpu::Backends::VULKAN]
        );
    }

    /// Draws with a real window renderer: on the machine's Direct3D 12
    /// adapter, or WARP, which CI installs.
    #[cfg(feature = "test-support")]
    mod direct3d_12 {
        use std::borrow::Cow;
        use std::sync::Once;

        use gpui::{
            AtlasKey, BorderStyle, Bounds, ContentMask, Corners, DevicePixels, Edges, FontId,
            GlyphId, Hsla, Quad, RenderGlyphParams, ScaledPixels, Scene, SubpixelSprite,
            TransformationMatrix, WindowBackgroundAppearance, point, px, size,
        };
        use gpui_wgpu::wgpu;
        use windows::{
            Win32::{
                Foundation::{HWND, LPARAM, LRESULT, WPARAM},
                System::LibraryLoader::GetModuleHandleW,
                UI::WindowsAndMessaging::{
                    CreateWindowExW, DefWindowProcW, DestroyWindow, RegisterClassW,
                    WINDOW_EX_STYLE, WNDCLASSW, WS_EX_NOREDIRECTIONBITMAP, WS_OVERLAPPEDWINDOW,
                },
            },
            core::w,
        };

        use super::super::{SHARED_GPU, WindowsRenderer};
        use crate::DirectXDevices;

        const WHITE: [u8; 4] = [255, 255, 255, 255];
        const RED: [u8; 4] = [255, 0, 0, 255];

        /// A hidden top-level window, created as `WindowsWindow` creates one.
        struct TestWindow(HWND);

        impl TestWindow {
            fn new(composited: bool) -> Self {
                static REGISTER: Once = Once::new();
                let instance = unsafe { GetModuleHandleW(None) }.expect("module handle");
                REGISTER.call_once(|| {
                    let class = WNDCLASSW {
                        lpfnWndProc: Some(window_procedure),
                        hInstance: instance.into(),
                        lpszClassName: w!("GpuiFastRendererTest"),
                        ..Default::default()
                    };
                    assert_ne!(unsafe { RegisterClassW(&class) }, 0);
                });
                let style = if composited {
                    WS_EX_NOREDIRECTIONBITMAP
                } else {
                    WINDOW_EX_STYLE(0)
                };
                let hwnd = unsafe {
                    CreateWindowExW(
                        style,
                        w!("GpuiFastRendererTest"),
                        w!(""),
                        WS_OVERLAPPEDWINDOW,
                        0,
                        0,
                        64,
                        48,
                        None,
                        None,
                        Some(instance.into()),
                        None,
                    )
                }
                .expect("creating a test window");
                Self(hwnd)
            }
        }

        impl Drop for TestWindow {
            fn drop(&mut self) {
                unsafe { DestroyWindow(self.0) }.ok();
            }
        }

        unsafe extern "system" fn window_procedure(
            hwnd: HWND,
            message: u32,
            wparam: WPARAM,
            lparam: LPARAM,
        ) -> LRESULT {
            unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
        }

        fn bounds(x: f32, y: f32, width: f32, height: f32) -> Bounds<ScaledPixels> {
            Bounds {
                origin: point(ScaledPixels(x), ScaledPixels(y)),
                size: size(ScaledPixels(width), ScaledPixels(height)),
            }
        }

        fn quad(bounds: Bounds<ScaledPixels>, color: Hsla) -> Quad {
            Quad {
                order: 0,
                border_style: BorderStyle::Solid,
                bounds,
                content_mask: ContentMask { bounds },
                background: color.into(),
                border_color: color,
                corner_radii: Corners::default(),
                border_widths: Edges::default(),
            }
        }

        fn assert_pixel(image: &image::RgbaImage, x: u32, y: u32, expected: [u8; 4]) {
            let actual = image.get_pixel(x, y).0;
            assert!(
                actual
                    .iter()
                    .zip(expected)
                    .all(|(actual, expected)| actual.abs_diff(expected) <= 3),
                "pixel ({x}, {y}) was {actual:?}, expected {expected:?}"
            );
        }

        fn red_quad_scene() -> Scene {
            let mut scene = Scene::default();
            scene.insert_primitive(quad(bounds(0.0, 0.0, 32.0, 48.0), gpui::red()));
            scene.finish();
            scene
        }

        #[test]
        fn window_renderers_draw_with_direct3d_12() {
            let devices = DirectXDevices::new().expect("creating Direct3D 11 devices");
            let window = TestWindow::new(true);
            let mut renderer = WindowsRenderer::new(window.0, &devices, false)
                .expect("creating a window renderer");
            assert!(renderer.composited, "the window lost DirectComposition");
            let backend = SHARED_GPU.with(|shared| {
                shared
                    .context
                    .borrow()
                    .as_ref()
                    .map(|context| context.adapter.get_info().backend)
            });
            assert_eq!(backend, Some(wgpu::Backend::Dx12));
            renderer.resize(size(DevicePixels(64), DevicePixels(48)));

            let mut empty = Scene::default();
            empty.finish();
            let image = renderer
                .render_to_image(&empty, WindowBackgroundAppearance::Opaque)
                .expect("rendering an opaque window");
            assert_pixel(&image, 32, 24, WHITE);
            let image = renderer
                .render_to_image(&empty, WindowBackgroundAppearance::Transparent)
                .expect("rendering a transparent window");
            assert_pixel(&image, 32, 24, [0, 0, 0, 0]);

            let scene = red_quad_scene();
            let image = renderer
                .render_to_image(&scene, WindowBackgroundAppearance::Opaque)
                .expect("rendering a quad");
            assert_pixel(&image, 16, 24, RED);
            assert_pixel(&image, 48, 24, WHITE);
            assert!(
                renderer.draw_base(&scene, WindowBackgroundAppearance::Opaque),
                "the frame was not presented into DirectComposition"
            );
            renderer.commit_if_pending();

            // A window without DirectComposition draws into its handle with the
            // same device.
            let plain_window = TestWindow::new(false);
            let mut plain = WindowsRenderer::new(plain_window.0, &devices, true)
                .expect("creating a window renderer without DirectComposition");
            assert!(!plain.composited);
            assert_eq!(plain.backends, wgpu::Backends::DX12);
            plain.resize(size(DevicePixels(64), DevicePixels(48)));
            let image = plain
                .render_to_image(&scene, WindowBackgroundAppearance::Opaque)
                .expect("rendering a quad without DirectComposition");
            assert_pixel(&image, 16, 24, RED);
            assert_pixel(&image, 48, 24, WHITE);
        }

        /// DirectWrite's ClearType coverage reaches the screen's subpixels in
        /// their order: coverage of the first subpixel only darkens red, or
        /// blue on a BGR panel.
        #[test]
        fn subpixel_glyphs_keep_their_channel_order() {
            let devices = DirectXDevices::new().expect("creating Direct3D 11 devices");
            let window = TestWindow::new(true);
            let mut renderer = WindowsRenderer::new(window.0, &devices, false)
                .expect("creating a window renderer");
            assert!(
                renderer.gpu.supports_dual_source_blending(),
                "subpixel text needs dual-source blending"
            );
            renderer.resize(size(DevicePixels(16), DevicePixels(16)));

            let key = AtlasKey::Glyph(RenderGlyphParams {
                font_id: FontId(0),
                glyph_id: GlyphId(1),
                font_size: px(16.0),
                subpixel_variant: point(0, 0),
                scale_factor: 1.0,
                is_emoji: false,
                subpixel_rendering: true,
                dilation: 0,
            });
            let coverage = [255u8, 0, 0, 0].repeat(16);
            let tile = renderer
                .sprite_atlas()
                .get_or_insert_with(key, &mut || {
                    Ok(Some((
                        size(DevicePixels(4), DevicePixels(4)),
                        Cow::Borrowed(coverage.as_slice()),
                    )))
                })
                .expect("inserting a glyph")
                .expect("a tile for the glyph");
            let sprite_bounds = bounds(0.0, 0.0, 16.0, 16.0);
            let mut scene = Scene::default();
            scene.insert_primitive(SubpixelSprite {
                order: 0,
                pad: 0,
                tile,
                bounds: sprite_bounds,
                content_mask: ContentMask {
                    bounds: sprite_bounds,
                },
                color: gpui::black(),
                transformation: TransformationMatrix::unit(),
            });
            scene.finish();

            let image = renderer
                .render_to_image(&scene, WindowBackgroundAppearance::Opaque)
                .expect("rendering a subpixel glyph");
            let [red, green, blue, _] = image.get_pixel(8, 8).0;
            let (covered, uncovered) = if WindowsRenderer::get_font_info().is_bgr {
                (blue, [red, green])
            } else {
                (red, [green, blue])
            };
            assert!(
                covered < 64 && uncovered.iter().all(|channel| *channel > 191),
                "pixel was {:?}",
                image.get_pixel(8, 8).0
            );
        }
    }
}
