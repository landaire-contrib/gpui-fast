//! Everything gpui-fast adds to the Windows platform lives under this module.
//!
//! The rest of this crate is kept as close to upstream GPUI (Zed's
//! `crates/gpui_windows`) as possible, so that pulling in upstream changes
//! stays a matter of merging rather than of rewriting. Upstream files only
//! *call into* this module: a field holding this module's state, a line
//! forwarding a call to it. The logic itself is written here, one file per
//! topic.
//!
//! `directx_renderer.rs` here is not one of these modules: `gpui_windows.rs`
//! redirects upstream's `directx_renderer` module to it.

pub(crate) mod class_names;
pub(crate) mod composition;
pub(crate) mod device_recovery;
pub(crate) mod device_wake;
pub(crate) mod font_fallback;
pub(crate) mod frame_schedule;
#[cfg(test)]
pub(crate) mod native_tests;
pub(crate) mod renderer;
