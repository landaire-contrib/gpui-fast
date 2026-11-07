// Modified by Longbridge for gpui-fast.
//! Skipping GPU tests on machines without a usable adapter, except where CI
//! requires one.

/// Reports a GPU test skipped for `reason`, or fails it when the
/// `GPUI_REQUIRE_GPU` environment variable is `1`: CI sets it on runners
/// whose GPU tests must not pass by skipping.
#[track_caller]
pub(crate) fn skip_without_gpu(reason: &str) {
    if std::env::var("GPUI_REQUIRE_GPU").is_ok_and(|value| value == "1") {
        panic!("GPUI_REQUIRE_GPU=1 but the test cannot run: {reason}");
    }
    eprintln!("skipped: {reason}");
}
