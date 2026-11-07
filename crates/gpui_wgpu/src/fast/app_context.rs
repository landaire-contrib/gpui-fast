//! The window renderer's wgpu context, published for applications.
//!
//! An application that renders with wgpu into a texture GPUI samples must use
//! the device GPUI draws with. The renderer publishes the context it creates
//! for the first window and the one it creates after device loss; the
//! generation tells an application when to recreate its GPU objects.

use parking_lot::Mutex;

use crate::WgpuContext;

/// Counts the contexts published in this process, starting at 1 for the
/// first. A new generation means a new device: GPU objects made on an earlier
/// generation's device cannot be used with it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WgpuContextGeneration(u64);

/// The context GPUI's window renderers draw with.
#[derive(Clone)]
pub struct WgpuAppContext {
    pub context: WgpuContext,
    pub generation: WgpuContextGeneration,
}

static PUBLISHED: Mutex<Option<WgpuAppContext>> = Mutex::new(None);

/// The context GPUI's window renderers draw with, once the first window has
/// created it.
///
/// Textures from `context.create_dynamic_texture` are sampled by GPUI without
/// a copy. When `context.device_lost()` turns true, stop using the device and
/// drop its objects; once a window recovers, this returns a new context with
/// a later generation, on which to recreate them.
pub fn app_context() -> Option<WgpuAppContext> {
    PUBLISHED.lock().clone()
}

/// Publishes a context a window renderer created.
pub(crate) fn publish(context: &WgpuContext) {
    let mut published = PUBLISHED.lock();
    let generation = published
        .as_ref()
        .map_or(1, |previous| previous.generation.0 + 1);
    *published = Some(WgpuAppContext {
        context: context.clone(),
        generation: WgpuContextGeneration(generation),
    });
}
