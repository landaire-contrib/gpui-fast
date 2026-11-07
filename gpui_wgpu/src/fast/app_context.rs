// Modified by Longbridge for gpui-fast.
//! The window renderer's wgpu context, published for applications.
//!
//! An application that renders with wgpu into a texture GPUI samples must use
//! the device GPUI draws with. The renderer publishes the context it creates
//! for the first window and the one it creates after device loss; the
//! generation tells an application when to recreate its GPU objects, and
//! [`on_device_lost`] tells it when to drop them.

use std::sync::Arc;

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

/// Why GPUI stopped drawing with a published context.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WgpuContextRetirement {
    /// The device was lost (driver reset, GPU removed, suspend). A window
    /// creates a new context when it recovers.
    DeviceLost,
    /// The platform released the GPU, for example while every window is
    /// hidden. A new context is published when the platform creates one.
    Released,
    /// A window created a new context while this one still worked, for
    /// example because the platform changed its own device.
    Replaced,
}

/// Passed to [`on_device_lost`] callbacks.
#[derive(Clone, Copy, Debug)]
pub struct WgpuContextRetired {
    /// The generation of the context GPUI no longer draws with.
    pub generation: WgpuContextGeneration,
    pub reason: WgpuContextRetirement,
}

type Callback = Arc<dyn Fn(&WgpuContextRetired) + Send + Sync>;

struct Published {
    current: Option<WgpuAppContext>,
    last_generation: u64,
}

static PUBLISHED: Mutex<Published> = Mutex::new(Published {
    current: None,
    last_generation: 0,
});

static CALLBACKS: Mutex<Vec<Callback>> = Mutex::new(Vec::new());

/// The context GPUI's window renderers draw with, once the first window has
/// created it. `None` before that, and from a device loss or release until a
/// window publishes a new context.
///
/// Textures from `context.create_dynamic_texture` are sampled by GPUI without
/// a copy. Drop GPU objects made on the device when [`on_device_lost`]
/// reports its generation retired, and make new ones on the context this
/// returns next, which has a later generation. CPU dynamic textures
/// (`Window::create_dynamic_texture`) move to the new device by themselves,
/// with their contents.
pub fn app_context() -> Option<WgpuAppContext> {
    PUBLISHED.lock().current.clone()
}

/// Registers `callback` to run when GPUI stops drawing with the published
/// context: the device was lost, the platform released the GPU, or a new
/// context replaced it.
///
/// It runs synchronously on the thread that detected it. For device loss
/// that is before GPUI drops its own objects and waits for the driver to
/// settle, so an application can drop everything it made on the device
/// first; a device is only freed once nothing holds it. Do not draw or
/// create GPU objects in the callback.
/// Callbacks run once per retired generation and stay registered for the
/// life of the process.
pub fn on_device_lost(callback: impl Fn(&WgpuContextRetired) + Send + Sync + 'static) {
    CALLBACKS.lock().push(Arc::new(callback));
}

/// Stops publishing the context, for a platform that releases its GPU.
///
/// Runs the [`on_device_lost`] callbacks with
/// [`WgpuContextRetirement::Released`] and moves CPU dynamic textures off the
/// device, keeping their contents; creating one fails until the platform
/// creates and publishes a context again. Has no effect when nothing is
/// published.
pub fn release_app_context() {
    retire(WgpuContextRetirement::Released, |_| true);
}

/// Publishes a context a window renderer created and moves CPU dynamic
/// textures onto its device. A context still published on another device is
/// retired as replaced.
pub(crate) fn publish(context: &WgpuContext) {
    let replaced = {
        let mut published = PUBLISHED.lock();
        published.last_generation += 1;
        let generation = WgpuContextGeneration(published.last_generation);
        let previous = published.current.replace(WgpuAppContext {
            context: context.clone(),
            generation,
        });
        // Under the lock, so a concurrent release cannot interleave.
        crate::fast::dynamic_textures::upload::use_context(context);
        previous
            .filter(|previous| previous.context.device.as_ref() != context.device.as_ref())
            .map(|previous| WgpuContextRetired {
                generation: previous.generation,
                reason: WgpuContextRetirement::Replaced,
            })
    };
    if let Some(replaced) = replaced {
        notify(&replaced);
    }
}

/// Called by a renderer about to recover from device loss: if the published
/// context's device is lost, retires it and tells the application.
pub(crate) fn device_lost() {
    retire(WgpuContextRetirement::DeviceLost, |published| {
        published.context.device_lost()
    });
}

fn retire(reason: WgpuContextRetirement, applies: impl FnOnce(&WgpuAppContext) -> bool) {
    let retired = {
        let mut published = PUBLISHED.lock();
        if !published.current.as_ref().is_some_and(applies) {
            return;
        }
        let Some(retired) = published.current.take() else {
            return;
        };
        crate::fast::dynamic_textures::upload::release();
        WgpuContextRetired {
            generation: retired.generation,
            reason,
        }
    };
    notify(&retired);
}

/// Runs the callbacks without holding any lock, so a callback may read the
/// published context or register another callback.
fn notify(retired: &WgpuContextRetired) {
    let callbacks = CALLBACKS.lock().clone();
    for callback in callbacks {
        callback(retired);
    }
}
