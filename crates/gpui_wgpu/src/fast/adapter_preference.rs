//! An application's choice of GPU adapter for GPUI's window renderer.
//!
//! Applications that render on GPUI's device (`crate::app_context`) can no
//! longer pick their own adapter, so they steer GPUI's instead. The
//! preference outranks `ZED_DEVICE_ID` and the compositor hint.

use parking_lot::Mutex;

use crate::WgpuContext;

/// Which adapter GPUI's window renderer prefers.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum WgpuAdapterPreference {
    /// GPUI's own ranking: discrete before integrated before software.
    #[default]
    SystemDefault,
    /// The first adapter whose name contains this text, ignoring case.
    NameContains(String),
    /// A software (CPU) adapter, such as llvmpipe or WARP.
    Software,
}

static PREFERENCE: Mutex<WgpuAdapterPreference> = Mutex::new(WgpuAdapterPreference::SystemDefault);

/// Sets the adapter preference for contexts created from now on.
///
/// Set it before the first window opens; it also applies when a window
/// recovers from device loss. When no adapter matches, GPUI uses its own
/// ranking and logs the miss.
pub fn set_adapter_preference(preference: WgpuAdapterPreference) {
    *PREFERENCE.lock() = preference;
}

/// Describes the adapters GPUI's window renderer chooses from, on the
/// backends it uses on this OS.
pub fn describe_adapters() -> Vec<wgpu::AdapterInfo> {
    let instance = WgpuContext::instance(None);
    gpui::block_on(instance.enumerate_adapters(wgpu::Backends::all()))
        .iter()
        .map(wgpu::Adapter::get_info)
        .collect()
}

/// Whether adapter selection rejects software adapters: never while the
/// application prefers one.
pub(crate) fn reject_software(requested: bool) -> bool {
    requested && *PREFERENCE.lock() != WgpuAdapterPreference::Software
}

/// Moves the adapters the preference selects to the front, keeping the
/// existing order otherwise.
pub(crate) fn apply(adapters: &mut [wgpu::Adapter]) {
    let preference = PREFERENCE.lock().clone();
    let name = match &preference {
        WgpuAdapterPreference::SystemDefault => return,
        WgpuAdapterPreference::NameContains(name) => name.to_lowercase(),
        WgpuAdapterPreference::Software => String::new(),
    };
    let matches = |adapter: &wgpu::Adapter| {
        let info = adapter.get_info();
        match preference {
            WgpuAdapterPreference::Software => info.device_type == wgpu::DeviceType::Cpu,
            _ => info.name.to_lowercase().contains(&name),
        }
    };
    adapters.sort_by_key(|adapter| !matches(adapter));
    if !adapters.first().is_some_and(matches) {
        log::warn!("No GPU adapter matches the preference {preference:?}; using the default order");
    }
}

#[cfg(all(test, feature = "test-support"))]
mod tests {
    use super::{WgpuAdapterPreference, apply, reject_software, set_adapter_preference};

    #[test]
    fn preference_orders_adapters_and_allows_software() {
        let instance = crate::fast::rounded_clip::headless_instance();
        let mut adapters = gpui::block_on(instance.enumerate_adapters(wgpu::Backends::all()));
        let Some(last) = adapters.last().map(|adapter| adapter.get_info().name) else {
            eprintln!("skipping: no GPU adapters");
            return;
        };

        set_adapter_preference(WgpuAdapterPreference::NameContains(last.to_uppercase()));
        apply(&mut adapters);
        assert_eq!(adapters[0].get_info().name, last);
        assert!(reject_software(true));

        // A miss keeps the order instead of failing.
        let order: Vec<_> = adapters
            .iter()
            .map(|adapter| adapter.get_info().name)
            .collect();
        set_adapter_preference(WgpuAdapterPreference::NameContains(
            "no such adapter".into(),
        ));
        apply(&mut adapters);
        let kept: Vec<_> = adapters
            .iter()
            .map(|adapter| adapter.get_info().name)
            .collect();
        assert_eq!(kept, order);

        set_adapter_preference(WgpuAdapterPreference::Software);
        assert!(!reject_software(true));
        set_adapter_preference(WgpuAdapterPreference::SystemDefault);
        assert!(reject_software(true));
        assert!(!reject_software(false));
    }
}
