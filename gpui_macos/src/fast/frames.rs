// Modified by Longbridge for gpui-fast.
//! What paces a window's frames: its display's `CVDisplayLink`, joined
//! through the registry in `display_link.rs`, and left while the window idles.
//!
//! wgpu acquires each frame's drawable from the window's layer itself, so a
//! `CAMetalDisplayLink`, which hands drawables to the frames it starts, would
//! compete with it; windows use the `CVDisplayLink` on every macOS version.
//! After an update in which no renderer presented, no frame is owed (a scene
//! to present again or repaint) and GPUI asked for no other frame, the window
//! leaves the link, so an idle window does not wake the main thread on every
//! vsync. GPUI's frame waker, and `schedule_frame`, rejoin it.

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::rc::{Rc, Weak};

use core_graphics::display::CGDirectDisplayID;
use gpui::{FrameRequestSource, RequestFrameOptions};
use gpui_util::ResultExt;
use scheduler::Instant;

use crate::WindowFrameSource;
use crate::renderer::Redraw;
use crate::window::MacWindowState;

/// A window's frame source, shared with GPUI's frame waker so that a wake
/// rejoins the link without taking the window's lock.
#[derive(Clone)]
pub(crate) struct WindowFrames(Rc<Frames>);

struct Frames {
    /// `None` once the window closed.
    source: RefCell<Option<WindowFrameSource>>,
    link: Cell<Link>,
    /// Whether GPUI asked for a frame since the current update began.
    woken: Cell<bool>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Link {
    /// Not subscribed: the window is occluded, closed or drawing
    /// synchronously.
    Stopped,
    Running(CGDirectDisplayID),
    /// Left while the window idles; a wake rejoins this display's link.
    Paused(CGDirectDisplayID),
}

impl WindowFrames {
    /// `callback(data)` runs on the main queue for each frame request.
    pub(crate) fn new(data: *mut c_void, callback: extern "C" fn(*mut c_void)) -> Self {
        Self(Rc::new(Frames {
            source: RefCell::new(Some(WindowFrameSource::new(data, callback))),
            link: Cell::new(Link::Stopped),
            woken: Cell::new(false),
        }))
    }

    /// Joins `display_id`'s link.
    pub(crate) fn start(&self, display_id: CGDirectDisplayID) {
        self.0.join(display_id);
    }

    /// Leaves the link until [`Self::start`].
    pub(crate) fn stop(&self) {
        if let Some(source) = self.0.source.borrow_mut().as_mut() {
            source.stop();
        }
        self.0.link.set(Link::Stopped);
    }

    /// Releases the frame source for good when the window closes: its
    /// dispatch source's context is the window's view, which may be freed
    /// after this, while wakers can outlive the window.
    pub(crate) fn close(&self) {
        drop(self.0.source.borrow_mut().take());
        self.0.link.set(Link::Stopped);
    }

    pub(crate) fn take_signal(&self) -> Option<(Instant, FrameRequestSource)> {
        self.0.source.borrow().as_ref()?.take_signal()
    }

    /// Notes that GPUI wants a frame, rejoining the link if it was left idle.
    pub(crate) fn wake(&self) {
        self.0.woken.set(true);
        if let Link::Paused(display_id) = self.0.link.get() {
            self.0.join(display_id);
        }
    }

    /// The frame waker GPUI holds; it does not keep the window alive.
    pub(crate) fn waker(&self) -> Rc<dyn Fn()> {
        let frames: Weak<Frames> = Rc::downgrade(&self.0);
        Rc::new(move || {
            if let Some(frames) = frames.upgrade() {
                WindowFrames(frames).wake();
            }
        })
    }
}

impl Frames {
    fn join(&self, display_id: CGDirectDisplayID) {
        // A wake while the source is being started or stopped finds it
        // borrowed; the link then is not paused, so nothing is missed.
        let Ok(mut source) = self.source.try_borrow_mut() else {
            return;
        };
        let Some(source) = source.as_mut() else {
            return;
        };
        let link = if source.start(display_id).log_err().is_some() {
            Link::Running(display_id)
        } else {
            Link::Stopped
        };
        self.link.set(link);
    }
}

/// Begins a display link update: the options its frame needs because of how
/// earlier frames went. A scene a renderer could not present is presented
/// again, and a renderer that recovered from device loss, or cleared its
/// atlas, has the scene repainted.
pub(crate) fn begin_frame(state: &mut MacWindowState) -> RequestFrameOptions {
    state.fast_frames.0.woken.set(false);
    let redraw = crate::fast::composition::renderers_mut(state)
        .map(|renderer| renderer.take_redraw())
        .fold(Redraw::None, Ord::max);
    RequestFrameOptions {
        require_presentation: redraw >= Redraw::Present,
        force_render: redraw == Redraw::Repaint,
        ..Default::default()
    }
}

/// Ends a display link update, leaving the link if the window idled.
pub(crate) fn end_frame(state: &mut MacWindowState) {
    let mut presented = false;
    let mut owed = false;
    for renderer in crate::fast::composition::renderers_mut(state) {
        presented |= renderer.take_presented();
        owed |= renderer.redraw_pending();
    }
    let frames = &state.fast_frames.0;
    if presented || owed || frames.woken.get() {
        return;
    }
    if let Link::Running(display_id) = frames.link.get()
        && let Some(source) = frames.source.borrow_mut().as_mut()
    {
        source.stop();
        frames.link.set(Link::Paused(display_id));
    }
}
