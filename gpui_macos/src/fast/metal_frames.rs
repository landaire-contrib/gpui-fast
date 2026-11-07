// Modified by Longbridge for gpui-fast.
//! Which source paces a window's frames.
//!
//! A `CAMetalDisplayLink` vends its layer's drawables to the frame it starts,
//! while `gpui_wgpu` acquires each frame's drawable from the layer itself
//! (`nextDrawable` in `Surface::get_current_texture`). The two would compete
//! for the layer's three drawables, so windows pace frames with the
//! `CVDisplayLink` registry on every macOS version.

/// Whether windows pace frames with a `CAMetalDisplayLink`: never.
pub(crate) fn available() -> bool {
    false
}
