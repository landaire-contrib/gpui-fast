// Modified by Longbridge for gpui-fast.
//! Renderers for windows whose platform creates the `wgpu::Surface` itself.
//!
//! Window handles are not the only surface targets: Windows composes through
//! a DirectComposition visual and macOS draws into a `CAMetalLayer`. The
//! platform implements [`WgpuSurfaceSource`] for its window, and the renderer
//! asks it for a surface on whichever instance owns the device: a new
//! instance for the first window and after device loss, the shared context's
//! instance otherwise.

use std::rc::Rc;
use std::sync::Arc;

use crate::fast::renderer_options::RendererOptions;
use crate::wgpu_renderer::RendererState;
use crate::{
    CompositorGpuHint, GpuContext, WgpuAtlas, WgpuContext, WgpuRenderer, WgpuSurfaceConfig,
};

/// The backends a native window renderer uses on this OS.
pub(crate) fn backends() -> wgpu::Backends {
    if cfg!(target_os = "windows") {
        wgpu::Backends::DX12
    } else if cfg!(target_vendor = "apple") {
        wgpu::Backends::METAL
    } else {
        wgpu::Backends::VULKAN | wgpu::Backends::GL
    }
}

/// Creates the surfaces of one native window.
///
/// Implemented by platform crates for their window type. A surface must be
/// created on the instance passed in: a device only accepts surfaces of the
/// instance it was created from.
pub trait WgpuSurfaceSource {
    /// Creates a surface for the window on `instance`.
    ///
    /// The implementation guarantees that the native objects it targets
    /// outlive the returned surface. A `SurfaceTargetUnsafe::RawHandle` target
    /// needs a display handle, either in the target or from
    /// [`Self::display_handle`]; layer and composition visual targets need
    /// none.
    fn create_surface(&self, instance: &wgpu::Instance) -> anyhow::Result<wgpu::Surface<'static>>;

    /// The display handle given to a new instance, for backends that need
    /// one to create surfaces (GL). DX12 and Metal do not.
    fn display_handle(&self) -> Option<Box<dyn wgpu::wgt::WgpuHasDisplayHandle>> {
        None
    }

    /// The backends of an instance created for this source: the first
    /// window's, and after device loss. Defaults to the OS's (DX12 on
    /// Windows, Metal on Apple targets, Vulkan and GL elsewhere). A window
    /// sharing an existing context gets its instance, whatever this says.
    fn backends(&self) -> wgpu::Backends {
        backends()
    }
}

impl WgpuContext {
    /// An instance with `backends`, like [`WgpuContext::instance`].
    pub fn instance_with_backends(
        backends: wgpu::Backends,
        display: Option<Box<dyn wgpu::wgt::WgpuHasDisplayHandle>>,
    ) -> wgpu::Instance {
        wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends,
            flags: wgpu::InstanceFlags::default(),
            backend_options: wgpu::BackendOptions::default(),
            memory_budget_thresholds: wgpu::MemoryBudgetThresholds::default(),
            display,
        })
    }
}

impl WgpuRenderer {
    /// Creates a renderer for a window whose surface `source` creates.
    ///
    /// The first renderer on `gpu_context` creates the context: it makes a
    /// new instance with the source's backends, has `source` create a
    /// surface on it, selects an adapter that can present to that surface
    /// (software adapters included) and publishes the context through
    /// [`crate::app_context`]. Later renderers share the context and get
    /// their surface from its instance.
    pub fn new_from_source(
        gpu_context: GpuContext,
        source: &dyn WgpuSurfaceSource,
        config: WgpuSurfaceConfig,
        compositor_gpu: Option<CompositorGpuHint>,
    ) -> anyhow::Result<Self> {
        let existing_instance = gpu_context
            .borrow()
            .as_ref()
            .map(|context| context.instance.clone());
        let instance = existing_instance.unwrap_or_else(|| {
            WgpuContext::instance_with_backends(source.backends(), source.display_handle())
        });
        let surface = source
            .create_surface(&instance)
            .map_err(|error| error.context("Failed to create surface"))?;

        let mut context_ref = gpu_context.borrow_mut();
        let surface_configured = context_ref.is_none();
        let context = match context_ref.as_mut() {
            Some(context) => {
                context.check_compatible_with_surface(&surface)?;
                context
            }
            None => {
                let context = context_ref.insert(WgpuContext::new(
                    instance,
                    &surface,
                    &config,
                    compositor_gpu,
                )?);
                crate::fast::app_context::publish(context);
                context
            }
        };

        let atlas = Arc::new(WgpuAtlas::from_context(context));
        Self::new_internal(
            Some(Rc::clone(&gpu_context)),
            context,
            surface,
            config,
            surface_configured,
            compositor_gpu,
            atlas,
        )
    }

    /// Recovers from a lost device, like [`WgpuRenderer::recover`], for a
    /// renderer made by [`Self::new_from_source`].
    ///
    /// The first window to recover runs the [`crate::on_device_lost`]
    /// callbacks, then creates a new instance and context, which is
    /// published through [`crate::app_context`] with a new generation; later
    /// windows adopt it. A software adapter is accepted only if the lost
    /// device was one, or the adapter preference asks for it. On error, keep
    /// the window and retry on a later frame: the GPU may need time to return
    /// after a reset or resume.
    pub fn recover_from_source(&mut self, source: &dyn WgpuSurfaceSource) -> anyhow::Result<()> {
        let gpu_context = Rc::clone(
            self.context
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("renderer has no shared GPU context"))?,
        );
        let needs_new_context = gpu_context
            .borrow()
            .as_ref()
            .is_none_or(|context| context.device_lost());
        let config = self.current_surface_config();

        let surface = if needs_new_context {
            log::warn!("GPU device lost, recreating context...");
            crate::fast::app_context::device_lost();
            if let Some(specs) = self.gpu_specs() {
                self.fast_options.software_adapter = specs.is_software_emulated;
            }
            // Release this renderer's device objects before creating another device.
            self.destroy();
            *gpu_context.borrow_mut() = None;
            // Give the driver time to settle after a reset before selecting an adapter.
            std::thread::sleep(std::time::Duration::from_millis(350));

            let instance =
                WgpuContext::instance_with_backends(source.backends(), source.display_handle());
            let surface = source.create_surface(&instance)?;
            let context = if self.fast_options.software_adapter {
                WgpuContext::new(instance, &surface, &config, self.compositor_gpu)?
            } else {
                WgpuContext::new_rejecting_software(
                    instance,
                    &surface,
                    &config,
                    self.compositor_gpu,
                )?
            };
            crate::fast::app_context::publish(&context);
            *gpu_context.borrow_mut() = Some(context);
            surface
        } else {
            let context_ref = gpu_context.borrow();
            let context = context_ref.as_ref().expect("checked above");
            source.create_surface(&context.instance)?
        };

        let context_ref = gpu_context.borrow();
        let context = context_ref.as_ref().expect("context was just installed");
        self.destroy();
        self.atlas.handle_device_lost(context);

        let is_bgr = self.is_bgr;
        let options = self.fast_options;
        *self = Self::new_internal(
            Some(Rc::clone(&gpu_context)),
            context,
            surface,
            config,
            needs_new_context,
            self.compositor_gpu,
            Arc::clone(&self.atlas),
        )?;
        self.set_subpixel_layout(is_bgr);
        RendererOptions::apply(options, self);
        log::info!("GPU recovery complete");
        Ok(())
    }

    /// Creates a renderer for another surface of the same window, like
    /// [`WgpuRenderer::new_sharing_atlas`], with the surface from `source`.
    pub fn new_sharing_atlas_from_source(
        &self,
        source: &dyn WgpuSurfaceSource,
        config: WgpuSurfaceConfig,
    ) -> anyhow::Result<Self> {
        let gpu_context = self
            .context
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("renderer has no shared GPU context"))?;
        let context_ref = gpu_context.borrow();
        let context = context_ref
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("GPU context is not initialized"))?;
        let surface = source.create_surface(&context.instance)?;
        context.check_compatible_with_surface(&surface)?;
        let mut renderer = Self::new_internal(
            Some(Rc::clone(gpu_context)),
            context,
            surface,
            config,
            false,
            self.compositor_gpu,
            Arc::clone(&self.atlas),
        )?;
        renderer.set_subpixel_layout(self.is_bgr);
        Ok(renderer)
    }

    /// Configures the current surface again with its current configuration
    /// and reports whether the driver accepted it.
    ///
    /// A failed configure (for example, out of memory creating a swap chain
    /// on a large resize) leaves the surface unconfigured, and `draw` then
    /// fails every frame. Call this after a resize whose frame did not draw,
    /// and retry on a later frame while it returns an error.
    pub fn reconfigure_surface(&mut self) -> anyhow::Result<()> {
        let RendererState::Ready { surface, core } = &self.state else {
            anyhow::bail!("the renderer has no configured surface");
        };
        let device = &core.resources.device;
        let out_of_memory = device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);
        let validation = device.push_error_scope(wgpu::ErrorFilter::Validation);
        surface.configure(device, &self.surface_config);
        let validation = gpui::block_on(validation.pop());
        let out_of_memory = gpui::block_on(out_of_memory.pop());
        match validation.or(out_of_memory) {
            Some(error) => anyhow::bail!("surface configuration failed: {error}"),
            None => Ok(()),
        }
    }

    /// The configuration that recreates the current surface.
    fn current_surface_config(&self) -> WgpuSurfaceConfig {
        WgpuSurfaceConfig {
            size: gpui::Size {
                width: gpui::DevicePixels(self.surface_config.width as i32),
                height: gpui::DevicePixels(self.surface_config.height as i32),
            },
            transparent: self.surface_config.alpha_mode != wgpu::CompositeAlphaMode::Opaque,
            preferred_present_mode: Some(self.surface_config.present_mode),
        }
    }
}
