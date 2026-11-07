// Modified by Longbridge for gpui-fast.
//! Window composition on DirectComposition: native views between GPUI's base
//! content and its overlays (zed-industries/zed#62379).
//!
//! The window's base visual, which wgpu presents into, sits at the root of
//! the window's target until GPUI orders the window's surfaces. The target's
//! root then becomes a visual of ours holding, in order, that base visual, a
//! visual per further GPUI surface (each presented into by a wgpu renderer of
//! its own that shares the window renderer's device and atlas), and the
//! visuals of native surfaces such as a WebView2 controller's. A native
//! surface is a visual with a rectangle clip, placed at its bounds and hidden
//! through its opacity.
//!
//! Recovering from a lost device recreates the window's DirectComposition
//! objects on the platform's new devices; [`recreate`] then rebuilds the tree
//! on them: new visuals and renderers for the GPUI surfaces, new visuals for
//! the native surfaces (whose owners are told through their
//! `compositor_recreated` callbacks), and the surfaces' order.

use std::{
    any::Any,
    cell::{Cell, RefCell},
    rc::{Rc, Weak},
};

use anyhow::{Context as _, Result};
use collections::{FxHashMap, FxHashSet};
use gpui::{
    Bounds, ComposedScene, CompositionSurfaceId, DevicePixels, PlatformCompositionSurface,
    PlatformCompositionSurfaceContent, PlatformSurfaceAttachment, Point, Scene,
    WindowBackgroundAppearance,
};
use gpui_util::ResultExt;
use gpui_wgpu::WgpuRenderer;
use windows::{
    Win32::Graphics::DirectComposition::{
        IDCompositionDevice, IDCompositionRectangleClip, IDCompositionVisual, IDCompositionVisual3,
    },
    core::{IUnknown, Interface},
};

use crate::fast::renderer::{
    DirectComposition, SurfaceTarget, WindowsRenderer, configure_text, surface_config,
};

/// The renderer's window composition state.
#[derive(Default)]
pub(crate) struct WindowComposition {
    /// The GPUI surfaces above the base one.
    overlays: FxHashMap<CompositionSurfaceId, Overlay>,
    /// The GPUI surface drawn by the window's own renderer, once GPUI has
    /// ordered the window's surfaces.
    base: Option<CompositionSurfaceId>,
    /// The surfaces in the order last set.
    surfaces: Vec<PlatformCompositionSurface>,
    /// The native surfaces created for the window that may still be alive.
    native_surfaces: Vec<Weak<RefCell<PortalState>>>,
    /// The target's root once the surfaces are ordered, on the current
    /// DirectComposition device.
    root: Option<IDCompositionVisual>,
    /// Counts composed frames, for overlays to tell whether they were drawn
    /// in the previous one.
    frame: u64,
}

/// A GPUI surface above the base one: its visual and the renderer presenting
/// into it.
struct Overlay {
    visual: IDCompositionVisual,
    renderer: WgpuRenderer,
    /// The composed frame that last drew onto it. A frame without a layer for
    /// the surface clears what the previous frame drew.
    last_drawn: Option<u64>,
}

struct Portal {
    state: Rc<RefCell<PortalState>>,
}

struct PortalState {
    comp_device: IDCompositionDevice,
    visual: IDCompositionVisual,
    clip: IDCompositionRectangleClip,
    bounds: Cell<Bounds<DevicePixels>>,
    parent_origin: Cell<Point<DevicePixels>>,
    visible: Cell<bool>,
    compositor_recreated_callback: RefCell<Option<Rc<dyn Fn(Box<dyn Any>) -> Result<()>>>>,
}

/// Forwarded to by `WindowsWindow::draw_composed`.
pub(crate) fn draw_composed(window: &crate::WindowsWindow, scene: ComposedScene<'_>) {
    let background_appearance = window.state.background_appearance.get();
    draw(
        &mut window.state.renderer.borrow_mut(),
        scene,
        background_appearance,
    )
    .log_err();
}

/// Forwarded to by `WindowsWindow::enable_window_composition`.
pub(crate) fn enable_window_composition(window: &crate::WindowsWindow) -> Result<()> {
    composition(&window.state.renderer.borrow()).map(|_| ())
}

/// Forwarded to by `WindowsWindow::create_native_surface`.
pub(crate) fn create_native_surface(
    window: &crate::WindowsWindow,
) -> Result<Rc<dyn PlatformSurfaceAttachment>> {
    let mut renderer = window.state.renderer.borrow_mut();
    let comp_device = composition(&renderer)?.comp_device.clone();
    let state = Rc::new(RefCell::new(PortalState::new(&comp_device)?));
    renderer
        .fast_composition
        .native_surfaces
        .push(Rc::downgrade(&state));
    Ok(Rc::new(Portal { state }))
}

/// Forwarded to by `WindowsWindow::set_composition_order`.
pub(crate) fn set_composition_order(
    window: &crate::WindowsWindow,
    surfaces: &[PlatformCompositionSurface],
) -> Result<()> {
    set_order(&mut window.state.renderer.borrow_mut(), surfaces)
}

/// Forwarded to by `WindowsRenderer::recover` before it releases the lost
/// devices: drops what was created on them.
pub(crate) fn release(renderer: &mut WindowsRenderer) {
    let state = &mut renderer.fast_composition;
    state.overlays.clear();
    state.root = None;
}

/// Forwarded to by `WindowsRenderer::recover` once it has recreated
/// DirectComposition and recovered its renderer: rebuilds the composition on
/// them.
pub(crate) fn recreate(renderer: &mut WindowsRenderer) -> Result<()> {
    let WindowsRenderer {
        gpu,
        composition,
        fast_composition: state,
        size,
        ..
    } = renderer;
    let overlay_ids = state
        .surfaces
        .iter()
        .filter(|surface| {
            matches!(surface.content, PlatformCompositionSurfaceContent::Gpui)
                && Some(surface.id) != state.base
        })
        .map(|surface| surface.id)
        .collect::<Vec<_>>();
    state
        .native_surfaces
        .retain(|surface| surface.strong_count() > 0);
    let Some(composition) = composition.as_ref() else {
        anyhow::ensure!(
            overlay_ids.is_empty(),
            "DirectComposition missing for GPUI surface"
        );
        return Ok(());
    };

    let root = unsafe { composition.comp_device.CreateVisual() }?;
    let mut overlays = FxHashMap::default();
    for surface in overlay_ids {
        overlays.insert(surface, Overlay::new(gpu, composition, *size)?);
    }
    for surface in &state.native_surfaces {
        let Some(live_surface) = surface.upgrade() else {
            continue;
        };
        PortalState::rebind(&mut live_surface.borrow_mut(), &composition.comp_device)
            .context("Recreating DirectComposition native surface")?;
    }
    state.overlays = overlays;
    if let Some(base_surface) = state.base
        && !state.surfaces.is_empty()
    {
        let platform_context = composition.comp_device.cast::<IUnknown>()?;
        for surface in &state.surfaces {
            match &surface.content {
                PlatformCompositionSurfaceContent::Native(platform_surface)
                | PlatformCompositionSurfaceContent::ExternalGpu(platform_surface) => {
                    platform_surface
                        .compositor_recreated(&platform_context)
                        .context("Rebinding platform composition surface")?;
                }
                PlatformCompositionSurfaceContent::Gpui => {}
            }
        }
        set_surface_order(
            composition,
            &root,
            &state.overlays,
            &state.surfaces,
            base_surface,
        )
        .context("Restoring DirectComposition surface order")?;
    }
    state.root = Some(root);
    Ok(())
}

/// Forwarded to by `WindowsRenderer::resize` once the base surface has its
/// new size: resizes the other GPUI surfaces.
pub(crate) fn resize(renderer: &mut WindowsRenderer) {
    for overlay in renderer.fast_composition.overlays.values_mut() {
        overlay.renderer.update_drawable_size(renderer.size);
    }
}

/// Forwarded to by `device_recovery::prepare_frame`: configures the other GPUI
/// surfaces again, as it does the base one.
pub(crate) fn reconfigure(renderer: &mut WindowsRenderer) -> Result<()> {
    for overlay in renderer.fast_composition.overlays.values_mut() {
        overlay.renderer.reconfigure_surface()?;
    }
    Ok(())
}

/// Forwarded to by `WindowsRenderer::take_needs_redraw`: whether an overlay's
/// renderer cleared the atlas it shares with the window's.
pub(crate) fn take_needs_redraw(renderer: &mut WindowsRenderer) -> bool {
    renderer
        .fast_composition
        .overlays
        .values_mut()
        .fold(false, |needs_redraw, overlay| {
            overlay.renderer.needs_redraw() || needs_redraw
        })
}

fn composition(renderer: &WindowsRenderer) -> Result<&DirectComposition> {
    renderer
        .composition
        .as_ref()
        .context("DirectComposition is disabled")
}

/// Draws each GPUI surface's part of `scene` onto its surface and presents
/// it. Until GPUI orders the surfaces, the whole scene is drawn as
/// `WindowsRenderer::draw` draws it.
fn draw(
    renderer: &mut WindowsRenderer,
    scene: ComposedScene<'_>,
    background_appearance: WindowBackgroundAppearance,
) -> Result<()> {
    let Some(base_surface) = renderer.fast_composition.base else {
        return renderer.draw(scene.scene(), background_appearance);
    };
    if !renderer.can_draw() || !crate::fast::device_recovery::prepare_frame(renderer) {
        return Ok(());
    }

    let frame = renderer.fast_composition.frame + 1;
    renderer.fast_composition.frame = frame;
    let mut base_presented = false;
    let mut all_presented = true;
    for layer in scene.layers() {
        let surface_scene = scene.layer_scene(layer)?;
        if layer.surface == base_surface {
            base_presented = renderer.draw_base(&surface_scene, background_appearance);
            all_presented &= base_presented;
            continue;
        }
        let overlay = renderer
            .fast_composition
            .overlays
            .get_mut(&layer.surface)
            .context("GPUI composition surface missing")?;
        all_presented &= overlay.renderer.draw(&surface_scene);
        overlay.last_drawn = Some(frame);
    }

    let mut empty = None;
    for overlay in renderer.fast_composition.overlays.values_mut() {
        if overlay.last_drawn.is_none_or(|drawn| drawn + 1 != frame) {
            continue;
        }
        let empty = empty.get_or_insert_with(|| {
            let mut scene = Scene::default();
            scene.finish();
            scene
        });
        all_presented &= overlay.renderer.draw(empty);
    }

    crate::fast::device_recovery::finish_frame(renderer, base_presented, all_presented);
    Ok(())
}

fn set_order(
    renderer: &mut WindowsRenderer,
    surfaces: &[PlatformCompositionSurface],
) -> Result<()> {
    let base = surfaces
        .iter()
        .find_map(|surface| match surface.content {
            PlatformCompositionSurfaceContent::Gpui => Some(surface.id),
            PlatformCompositionSurfaceContent::Native(_)
            | PlatformCompositionSurfaceContent::ExternalGpu(_) => None,
        })
        .context("composition has no GPUI base surface")?;
    let WindowsRenderer {
        gpu,
        composition,
        fast_composition: state,
        size,
        ..
    } = renderer;
    let composition = composition
        .as_ref()
        .context("DirectComposition is disabled")?;
    let WindowComposition {
        overlays,
        base: previous_base,
        surfaces: previous_surfaces,
        root,
        ..
    } = state;
    let root = match root {
        Some(root) => root,
        None => root.insert(unsafe { composition.comp_device.CreateVisual() }?),
    };

    for surface in surfaces {
        if !matches!(surface.content, PlatformCompositionSurfaceContent::Gpui) {
            continue;
        }
        if surface.id == base || overlays.contains_key(&surface.id) {
            continue;
        }
        overlays.insert(surface.id, Overlay::new(gpu, composition, *size)?);
    }
    let active_gpui_surfaces = surfaces
        .iter()
        .filter_map(|surface| match surface.content {
            PlatformCompositionSurfaceContent::Gpui if surface.id != base => Some(surface.id),
            _ => None,
        })
        .collect::<FxHashSet<_>>();
    if let Err(error) = set_surface_order(composition, root, overlays, surfaces, base) {
        if let Some(previous_base) = *previous_base
            && !previous_surfaces.is_empty()
            && let Err(rollback_error) = set_surface_order(
                composition,
                root,
                overlays,
                previous_surfaces,
                previous_base,
            )
        {
            return Err(error).context(format!(
                "restoring the previous DirectComposition tree also failed: {rollback_error:#}"
            ));
        }
        return Err(error);
    }
    overlays.retain(|id, _| active_gpui_surfaces.contains(id));
    *previous_base = Some(base);
    *previous_surfaces = surfaces.to_vec();
    Ok(())
}

/// Makes `root` hold `surfaces`' visuals in order, each nested in its
/// parent's, with `base_surface` drawn by the window's base visual.
fn set_surface_order(
    composition: &DirectComposition,
    root: &IDCompositionVisual,
    overlays: &FxHashMap<CompositionSurfaceId, Overlay>,
    surfaces: &[PlatformCompositionSurface],
    base_surface: CompositionSurfaceId,
) -> Result<()> {
    let visuals = surfaces
        .iter()
        .map(|surface| match &surface.content {
            PlatformCompositionSurfaceContent::Gpui if surface.id == base_surface => {
                Ok((surface.id, composition.comp_visual.clone()))
            }
            PlatformCompositionSurfaceContent::Gpui => overlays
                .get(&surface.id)
                .map(|overlay| (surface.id, overlay.visual.clone()))
                .context("GPUI composition visual is missing"),
            PlatformCompositionSurfaceContent::Native(platform_surface)
            | PlatformCompositionSurfaceContent::ExternalGpu(platform_surface) => {
                let handle = platform_surface.platform_handle()?;
                let unknown = handle.downcast::<IUnknown>().map_err(|_| {
                    anyhow::anyhow!("native surface is not a DirectComposition visual")
                })?;
                unknown
                    .cast::<IDCompositionVisual>()
                    .map(|visual| (surface.id, visual))
                    .map_err(Into::into)
            }
        })
        .collect::<Result<Vec<_>>>()?;
    let visuals_by_id = visuals.iter().cloned().collect::<FxHashMap<_, _>>();
    for surface in surfaces {
        match &surface.content {
            PlatformCompositionSurfaceContent::Gpui => {
                let visual = visuals_by_id
                    .get(&surface.id)
                    .context("GPUI composition visual is missing")?;
                unsafe {
                    visual.SetOffsetX2(
                        (surface.window_origin.x - surface.parent_origin.x).0 as f32,
                    )?;
                    visual.SetOffsetY2(
                        (surface.window_origin.y - surface.parent_origin.y).0 as f32,
                    )?;
                }
            }
            PlatformCompositionSurfaceContent::Native(platform_surface)
            | PlatformCompositionSurfaceContent::ExternalGpu(platform_surface) => {
                platform_surface.set_parent_origin(surface.parent_origin)?;
            }
        }
    }
    unsafe {
        // The base visual is the target's root until the first order.
        composition.comp_target.SetRoot(root)?;
        root.RemoveAllVisuals()?;
        for surface in surfaces {
            if matches!(surface.content, PlatformCompositionSurfaceContent::Gpui) {
                visuals_by_id
                    .get(&surface.id)
                    .context("GPUI composition visual is missing")?
                    .RemoveAllVisuals()?;
            }
        }
        let mut previous_by_parent = FxHashMap::default();
        for surface in surfaces {
            let visual = visuals_by_id
                .get(&surface.id)
                .context("composition visual is missing")?;
            let parent = surface.parent.and_then(|parent| visuals_by_id.get(&parent));
            let container = parent.unwrap_or(root);
            let previous = previous_by_parent.get(&surface.parent);
            container.AddVisual(visual, true, previous)?;
            previous_by_parent.insert(surface.parent, visual.clone());
        }
    }
    composition.commit()
}

impl Overlay {
    /// A visual for another GPUI surface of the window, and a renderer on
    /// `gpu`'s device and atlas presenting into it.
    fn new(
        gpu: &WgpuRenderer,
        composition: &DirectComposition,
        size: gpui::Size<DevicePixels>,
    ) -> Result<Self> {
        let visual = unsafe { composition.comp_device.CreateVisual() }?;
        let mut renderer = gpu
            .new_sharing_atlas_from_source(
                &SurfaceTarget::Visual(visual.clone()),
                surface_config(size, true),
            )
            .context("Creating GPUI composition surface renderer")?;
        configure_text(&mut renderer);
        composition.commit()?;
        Ok(Self {
            visual,
            renderer,
            last_drawn: None,
        })
    }
}

impl PortalState {
    fn new(comp_device: &IDCompositionDevice) -> Result<Self> {
        let visual = unsafe { comp_device.CreateVisual() }?;
        let clip = unsafe { comp_device.CreateRectangleClip() }?;
        unsafe {
            visual.SetClip(&clip)?;
        }
        Ok(Self {
            comp_device: comp_device.clone(),
            visual,
            clip,
            bounds: Cell::new(Bounds::default()),
            parent_origin: Cell::new(Point::default()),
            visible: Cell::new(true),
            compositor_recreated_callback: RefCell::new(None),
        })
    }

    /// Replaces `state`'s visual with one on `comp_device`, placed, clipped
    /// and shown as the old one was.
    fn rebind(state: &mut Self, comp_device: &IDCompositionDevice) -> Result<()> {
        let bounds = state.bounds.get();
        let parent_origin = state.parent_origin.get();
        let visible = state.visible.get();
        let compositor_recreated_callback = state.compositor_recreated_callback.borrow().clone();
        let replacement = Self::new(comp_device)?;
        let x = (bounds.origin.x - parent_origin.x).0 as f32;
        let y = (bounds.origin.y - parent_origin.y).0 as f32;
        let width = bounds.size.width.0.max(0) as f32;
        let height = bounds.size.height.0.max(0) as f32;
        unsafe {
            replacement.visual.SetOffsetX2(x)?;
            replacement.visual.SetOffsetY2(y)?;
            replacement.clip.SetLeft2(0.0)?;
            replacement.clip.SetTop2(0.0)?;
            replacement.clip.SetRight2(width)?;
            replacement.clip.SetBottom2(height)?;
            replacement
                .visual
                .cast::<IDCompositionVisual3>()?
                .SetOpacity2(if visible { 1.0 } else { 0.0 })?;
        }
        replacement.bounds.set(bounds);
        replacement.parent_origin.set(parent_origin);
        replacement.visible.set(visible);
        *replacement.compositor_recreated_callback.borrow_mut() = compositor_recreated_callback;
        *state = replacement;
        Ok(())
    }
}

impl PlatformSurfaceAttachment for Portal {
    fn set_bounds(&self, bounds: Bounds<DevicePixels>) -> Result<()> {
        let state = self.state.borrow();
        let parent_origin = state.parent_origin.get();
        let x = (bounds.origin.x - parent_origin.x).0 as f32;
        let y = (bounds.origin.y - parent_origin.y).0 as f32;
        let width = bounds.size.width.0.max(0) as f32;
        let height = bounds.size.height.0.max(0) as f32;
        unsafe {
            state.visual.SetOffsetX2(x)?;
            state.visual.SetOffsetY2(y)?;
            state.clip.SetLeft2(0.0)?;
            state.clip.SetTop2(0.0)?;
            state.clip.SetRight2(width)?;
            state.clip.SetBottom2(height)?;
            state.comp_device.Commit()?;
        }
        state.bounds.set(bounds);
        Ok(())
    }

    fn bounds(&self) -> Bounds<DevicePixels> {
        self.state.borrow().bounds.get()
    }

    fn set_parent_origin(&self, origin: Point<DevicePixels>) -> Result<()> {
        let bounds = {
            let state = self.state.borrow();
            state.parent_origin.set(origin);
            state.bounds.get()
        };
        self.set_bounds(bounds)
    }

    fn set_visible(&self, visible: bool) -> Result<()> {
        let state = self.state.borrow();
        if state.visible.get() != visible {
            unsafe {
                state
                    .visual
                    .cast::<IDCompositionVisual3>()?
                    .SetOpacity2(if visible { 1.0 } else { 0.0 })?;
                state.comp_device.Commit()?;
            }
            state.visible.set(visible);
        }
        Ok(())
    }

    fn compositor_recreated(&self, _platform_context: &dyn Any) -> Result<()> {
        let callback = self
            .state
            .borrow()
            .compositor_recreated_callback
            .borrow()
            .clone();
        if let Some(callback) = callback {
            callback(self.platform_handle()?)?;
        }
        Ok(())
    }

    fn set_compositor_recreated_callback(
        &self,
        callback: Rc<dyn Fn(Box<dyn Any>) -> Result<()>>,
    ) -> Result<()> {
        *self
            .state
            .borrow()
            .compositor_recreated_callback
            .borrow_mut() = Some(callback);
        Ok(())
    }

    fn platform_handle(&self) -> Result<Box<dyn Any>> {
        Ok(Box::new(self.state.borrow().visual.cast::<IUnknown>()?))
    }
}
