// Modified by Longbridge for gpui-fast.
//! Everything gpui-fast adds to the platform selection crate lives under this
//! module.
//!
//! The rest of this crate is kept as close to upstream GPUI (Zed's
//! `crates/gpui_platform`) as possible. Upstream files only call into this
//! module; the logic itself is written here, one file per topic.

#[cfg(any(feature = "bench-support", feature = "test-support"))]
pub(crate) mod headless;
