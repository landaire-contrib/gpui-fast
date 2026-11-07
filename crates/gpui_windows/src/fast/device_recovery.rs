//! Keeps a window alive when its renderer fails to resize or to recover from a lost device.
//!
//! Upstream treats any failed resize as a lost device, and panics when a window's renderer
//! cannot be rebuilt on the devices the platform recreated. Both crash in the field:
//!
//! - a resize that runs out of memory (`E_OUTOFMEMORY`) on a healthy device leaves the
//!   surface without a swap chain, and drawing fails every frame;
//! - after a GPU reset (`DXGI_ERROR_DEVICE_RESET`), the renderer's retries all reuse the one set
//!   of devices the platform recreated, which a driver still recovering may lose again.
//!
//! So the surfaces are configured again before the next frame after a resize whose frame did
//! not present, the frame is skipped while that fails, a failed rebuild asks the platform for
//! new devices a few times before giving up, and a renderer left without its devices skips
//! drawing.

use std::sync::atomic::Ordering;

use anyhow::Error;
use gpui::{DevicePixels, Size};

use crate::fast::renderer::WindowsRenderer;
use crate::{DirectXDevices, WindowsWindowInner};

/// How many times in a row a window asks the platform for new devices before it gives up.
const MAX_FAILED_RECOVERIES: u32 = 5;

#[derive(Default)]
pub(crate) struct RecoveryState {
    /// Rebuilds that failed since the window last recovered.
    failed_recoveries: u32,
    /// Whether the surfaces were resized since a frame last presented on all of them.
    resized: bool,
    /// Whether a frame after a resize did not present, which a surface wgpu failed to
    /// configure causes: the surfaces are configured again before the next frame draws.
    reconfigure: bool,
}

impl RecoveryState {
    /// The renderer recovered, with surfaces configured anew.
    fn recovered(&mut self) {
        *self = Self::default();
    }

    /// Counts a failed rebuild; `false` once the window has used up its attempts.
    fn failed(&mut self) -> bool {
        self.failed_recoveries += 1;
        self.failed_recoveries < MAX_FAILED_RECOVERIES
    }
}

/// Forwarded to by `WindowsWindowInner::handle_size_change`.
pub(crate) fn resize(window: &WindowsWindowInner, size: Size<DevicePixels>) {
    let mut renderer = window.state.renderer.borrow_mut();
    if renderer.resize(size) {
        renderer.fast_recovery.resized = true;
    }
}

/// Called before a frame draws: configures the surfaces again if a frame after a resize did
/// not present. Returns `false`, and the frame is skipped, while that fails; the next frame
/// retries it.
pub(crate) fn prepare_frame(renderer: &mut WindowsRenderer) -> bool {
    if !renderer.fast_recovery.reconfigure {
        return true;
    }

    let reconfigured = renderer
        .gpu
        .reconfigure_surface()
        .and_then(|()| crate::fast::composition::reconfigure(renderer));
    if let Err(error) = reconfigured {
        log::error!(
            "Failed to configure the window's surfaces at {}x{}, retrying at the next frame: \
             {error:#}",
            renderer.size.width.0,
            renderer.size.height.0,
        );
        return false;
    }
    renderer.fast_recovery.reconfigure = false;
    renderer.commit_pending = true;
    true
}

/// Called once a frame has drawn: shows the swap chains wgpu made once the base surface
/// presented, and has the surfaces configured again if a frame after a resize did not present.
pub(crate) fn finish_frame(
    renderer: &mut WindowsRenderer,
    base_presented: bool,
    all_presented: bool,
) {
    let state = &mut renderer.fast_recovery;
    if all_presented {
        state.resized = false;
    } else {
        state.reconfigure |= state.resized;
        // A surface wgpu skipped may have been configured again, with a new swap chain.
        renderer.commit_pending = true;
    }
    if base_presented {
        renderer.commit_if_pending();
    }
}

/// Forwarded to by `WindowsWindowInner::handle_device_lost`: rebuilds the renderer on the
/// platform's new devices, and returns whether it can draw again.
pub(crate) fn recover(window: &WindowsWindowInner, devices: &DirectXDevices) -> bool {
    let mut renderer = window.state.renderer.borrow_mut();
    let Err(error) = renderer.handle_device_lost(devices) else {
        renderer.fast_recovery.recovered();
        return true;
    };
    let device_lost = unsafe { devices.device.GetDeviceRemovedReason() }.err();
    let retry = renderer.fast_recovery.failed();
    log::error!(
        "Failed to recover renderer from lost device (attempt {} of {MAX_FAILED_RECOVERIES}), \
         new device lost: {device_lost:?}: {error:#}",
        renderer.fast_recovery.failed_recoveries,
    );
    if !retry {
        recovery_failed(error);
    }
    drop(renderer);
    window
        .state
        .invalidate_devices
        .store(true, Ordering::Release);
    crate::fast::frame_schedule::FrameRequest::request(&window.state.fast_frame_request);
    false
}

/// Panics with the whole chain of `error`, which names the step that failed and its `HRESULT`.
pub(crate) fn recovery_failed(error: Error) -> ! {
    panic!("Device lost: {error:#}");
}

#[cfg(test)]
mod tests {
    use super::{MAX_FAILED_RECOVERIES, RecoveryState};

    #[test]
    fn a_window_gives_up_after_its_failed_recoveries() {
        let mut state = RecoveryState::default();
        for _ in 1..MAX_FAILED_RECOVERIES {
            assert!(state.failed());
        }
        assert!(!state.failed());
    }

    #[test]
    fn recovering_resets_the_attempts() {
        let mut state = RecoveryState::default();
        for _ in 1..MAX_FAILED_RECOVERIES {
            assert!(state.failed());
        }
        state.recovered();
        assert!(state.failed());
    }
}
