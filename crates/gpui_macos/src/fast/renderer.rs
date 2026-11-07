//! The renderer of a macOS window or composition overlay: `gpui_wgpu` drawing
//! into a `CAMetalLayer` the view hosts.
//!
//! This module is the crate's `renderer` (redirected from `gpui_macos.rs`),
//! and keeps the interface `window.rs` used on `gpui_apple`'s Metal renderer:
//! a layer to host, its drawable size, opacity and transaction mode, and a
//! sprite atlas for text and images. The layer belongs to this renderer, and
//! wgpu presents into it through `SurfaceTargetUnsafe::CoreAnimationLayer`.
//!
//! wgpu acquires each frame's drawable itself, so windows pace frames with
//! the `CVDisplayLink` registry (`fast::frames`). Frames that wgpu could not
//! present (the window was occluded, the surface was being reconfigured) or
//! that need repainting (the device was recovered) are requested again
//! through [`Renderer::take_redraw`].

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context as _, Result};
use gpui::{DevicePixels, Scene, Size};
use gpui_wgpu::{GpuContext, WgpuAtlas, WgpuRenderer, WgpuSurfaceConfig, WgpuSurfaceSource, wgpu};
use objc2::rc::Retained;
use objc2_quartz_core::{CAAutoresizingMask, CAMetalLayer};

/// The GPU context shared by every window of the process.
pub(crate) type Context = GpuContext;

pub(crate) fn new_renderer(
    context: self::Context,
    transparent: bool,
    use_display_p3: bool,
) -> Result<Renderer> {
    let layer = MetalLayer::new();
    let config = surface_config(None, transparent);
    let wgpu = WgpuRenderer::new_from_source(context, &layer, config, None)?;
    let mut renderer = Renderer {
        layer,
        wgpu,
        overlay: false,
        display_p3: use_display_p3,
        redraw: Redraw::None,
        presented: false,
    };
    renderer.apply_color_space();
    renderer.wgpu.set_clear_color(clear_color(transparent));
    Ok(renderer)
}

/// A renderer for a composition overlay above `base`: transparent, on a layer
/// of its own, on `base`'s device and sharing its sprite atlas, so tiles
/// rasterized for one surface are valid on every surface of the window.
pub(crate) fn new_overlay_renderer(base: &Renderer) -> Result<Renderer> {
    let layer = MetalLayer::new();
    let config = surface_config(Some(base.wgpu.viewport_size()), true);
    let wgpu = base.wgpu.new_sharing_atlas_from_source(&layer, config)?;
    let mut renderer = Renderer {
        layer,
        wgpu,
        overlay: true,
        display_p3: base.display_p3,
        redraw: Redraw::None,
        presented: false,
    };
    renderer.apply_color_space();
    Ok(renderer)
}

/// What the next frame must do because of an earlier frame's outcome.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Redraw {
    #[default]
    None,
    /// Present the last drawn scene again: the frame drawing it was dropped.
    Present,
    /// Draw again from scratch: the sprite atlas was cleared, so the scene's
    /// tiles no longer refer to its contents.
    Repaint,
}

pub(crate) struct Renderer {
    layer: MetalLayer,
    wgpu: WgpuRenderer,
    /// Whether this renderer draws a composition overlay, which recovers from
    /// device loss by rejoining the window renderer ([`Renderer::rejoin`]).
    overlay: bool,
    display_p3: bool,
    redraw: Redraw,
    /// Whether a frame was presented since [`Renderer::take_presented`].
    presented: bool,
}

impl Renderer {
    pub(crate) fn layer_ptr(&self) -> *mut CAMetalLayer {
        Retained::as_ptr(&self.layer.0).cast_mut()
    }

    pub(crate) fn sprite_atlas(&self) -> &Arc<WgpuAtlas> {
        self.wgpu.sprite_atlas()
    }

    /// wgpu reads the layer's mode whenever it acquires a drawable, and
    /// presents in a Core Animation transaction while it is set.
    pub(crate) fn set_presents_with_transaction(&mut self, presents_with_transaction: bool) {
        self.layer
            .0
            .setPresentsWithTransaction(presents_with_transaction);
    }

    pub(crate) fn update_drawable_size(&mut self, size: Size<DevicePixels>) {
        self.wgpu.update_drawable_size(size);
    }

    pub(crate) fn update_transparency(&mut self, transparent: bool) {
        self.wgpu.update_transparency(transparent);
        self.wgpu.set_clear_color(clear_color(transparent));
    }

    pub(crate) fn destroy(&mut self) {
        self.wgpu.destroy();
    }

    pub(crate) fn draw(&mut self, scene: &Scene) {
        if self.wgpu.device_lost() {
            if self.overlay {
                self.request(Redraw::Repaint);
            } else {
                self.recover();
            }
            return;
        }
        warn_about_paint_surfaces(scene);
        if self.wgpu.draw(scene) {
            self.presented = true;
        } else {
            self.request(Redraw::Present);
        }
        if self.wgpu.needs_redraw() {
            self.request(Redraw::Repaint);
        }
    }

    /// Whether the device this renderer draws with was lost.
    pub(crate) fn device_lost(&self) -> bool {
        self.wgpu.device_lost()
    }

    /// Recovers from a lost device, onto a new device when this is the first
    /// renderer of the process to notice, or onto the one another already
    /// recovered onto, keeping its color space. Either way the next frame
    /// repaints, also to retry a recovery that failed.
    pub(crate) fn recover(&mut self) -> bool {
        self.request(Redraw::Repaint);
        match self.wgpu.recover_from_source(&self.layer) {
            Ok(()) => true,
            Err(error) => {
                log::warn!("GPU recovery failed, will retry on next frame: {error:#}");
                false
            }
        }
    }

    /// Moves this overlay renderer onto `base`'s device and atlas after
    /// `base` recovered from a lost device.
    pub(crate) fn rejoin(&mut self, base: &Renderer) -> Result<()> {
        let config = surface_config(Some(self.wgpu.viewport_size()), true);
        // Release the lost device's surface on the layer before making another.
        self.wgpu.destroy();
        self.wgpu = base
            .wgpu
            .new_sharing_atlas_from_source(&self.layer, config)?;
        self.apply_color_space();
        Ok(())
    }

    /// What the next frame must do because of this renderer's earlier
    /// frames, clearing it.
    pub(crate) fn take_redraw(&mut self) -> Redraw {
        std::mem::take(&mut self.redraw)
    }

    /// Whether a later frame must draw or present again, without clearing it.
    pub(crate) fn redraw_pending(&self) -> bool {
        self.redraw != Redraw::None
    }

    /// Whether this renderer presented a frame since the last call.
    pub(crate) fn take_presented(&mut self) -> bool {
        std::mem::take(&mut self.presented)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn render_to_image(&mut self, scene: &Scene) -> Result<image::RgbaImage> {
        self.wgpu.render_to_image(scene)
    }

    fn request(&mut self, redraw: Redraw) {
        self.redraw = self.redraw.max(redraw);
    }

    /// Draws in Display P3 when the window asked for it
    /// (`WindowParams::use_display_p3`), as overlays above such a window do.
    fn apply_color_space(&mut self) {
        if self.display_p3 {
            self.wgpu
                .set_color_space(wgpu::SurfaceColorSpace::DisplayP3);
        }
    }
}

/// The `CAMetalLayer` a renderer draws into, hosted by a window's view or an
/// overlay view.
struct MetalLayer(Retained<CAMetalLayer>);

impl MetalLayer {
    fn new() -> Self {
        let layer = CAMetalLayer::new();
        // wgpu sets the device, pixel format, drawable size and count, and
        // opacity whenever it configures the surface; what remains is how the
        // layer follows its view.
        layer.setNeedsDisplayOnBoundsChange(true);
        layer.setAutoresizingMask(
            CAAutoresizingMask::LayerWidthSizable | CAAutoresizingMask::LayerHeightSizable,
        );
        Self(layer)
    }
}

impl WgpuSurfaceSource for MetalLayer {
    fn create_surface(&self, instance: &wgpu::Instance) -> Result<wgpu::Surface<'static>> {
        let layer = Retained::as_ptr(&self.0).cast_mut().cast();
        // SAFETY: `layer` is a live `CAMetalLayer`. wgpu's Metal surface
        // retains the layer it is created for, so the layer outlives it.
        unsafe {
            instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::CoreAnimationLayer(layer))
        }
        .context("creating a wgpu surface for a CAMetalLayer")
    }
}

/// The configuration a new surface starts with. Without `size`, a
/// placeholder until the view reports its drawable size, which it does before
/// its first frame (`viewDidChangeBackingProperties`, `setFrameSize:`).
fn surface_config(size: Option<Size<DevicePixels>>, transparent: bool) -> WgpuSurfaceConfig {
    WgpuSurfaceConfig {
        size: size.unwrap_or(Size {
            width: DevicePixels(1),
            height: DevicePixels(1),
        }),
        transparent,
        preferred_present_mode: None,
    }
}

/// What each frame starts from: opaque black in an opaque window, so its
/// captures (`render_to_image`) have no transparent pixels.
fn clear_color(transparent: bool) -> wgpu::Color {
    if transparent {
        wgpu::Color::TRANSPARENT
    } else {
        wgpu::Color::BLACK
    }
}

/// `gpui_wgpu` draws no `PaintSurface` (a `CVPixelBuffer` painted with
/// `gpui::surface`, as screen capture frames and video are): say so once
/// rather than leave the element blank without a trace.
fn warn_about_paint_surfaces(scene: &Scene) {
    static WARNED: AtomicBool = AtomicBool::new(false);
    if !scene.surfaces.is_empty() && !WARNED.swap(true, Ordering::Relaxed) {
        log::warn!(
            "gpui::surface elements (CVPixelBuffer content) are not drawn: \
             the wgpu renderer does not support PaintSurface"
        );
    }
}
