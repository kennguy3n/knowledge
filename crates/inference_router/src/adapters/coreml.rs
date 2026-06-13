//! Apple **Core ML / ANE** inference adapter.
//!
//! Routes SLM work to a Core ML model executed on the Apple Neural
//! Engine (ANE) when the model graph is ANE-resident, falling back to
//! the GPU/CPU compute units Core ML itself chooses otherwise. The heavy
//! runtime lives in the iOS / macOS Swift shell (which links
//! `CoreML.framework` and owns the compiled `.mlmodelc`); this crate
//! holds the routing seam — a host-injected
//! [`AcceleratorBackend`](crate::adapters::accelerator::AcceleratorBackend) —
//! exactly like the MLX adapter. See
//! [`crate::adapters::accelerator`] for the shared dispatch, capability
//! detection, and determinism-contract logic.
//!
//! Compiled only under the `coreml` feature so a build that does not
//! target Apple accelerators carries none of this code.

use crate::adapter::AdapterKind;
use crate::adapters::accelerator::{AcceleratorAdapter, AcceleratorClass};

/// Marker type identifying the Core ML / ANE accelerator.
pub struct CoreMl;

impl AcceleratorClass for CoreMl {
    const KIND: AdapterKind = AdapterKind::CoreMl;

    /// Core ML exists only on Apple platforms. The ANE specifically is
    /// Apple-silicon (`aarch64`) macOS and all modern iOS devices; we
    /// gate on the OS here and let the injected backend report the
    /// dynamic runtime/hardware presence via
    /// [`AcceleratorBackend::capabilities`](crate::adapters::accelerator::AcceleratorBackend::capabilities).
    fn platform_supported() -> bool {
        cfg!(any(
            all(target_arch = "aarch64", target_os = "macos"),
            target_os = "ios"
        ))
    }
}

/// Apple Core ML / ANE adapter. A concrete
/// [`AcceleratorAdapter`](crate::adapters::accelerator::AcceleratorAdapter)
/// specialised for the [`CoreMl`] class.
///
/// Construct with
/// [`CoreMlAdapter::new(config, backend)`](crate::adapters::accelerator::AcceleratorAdapter::new),
/// where `backend` is the host's Core ML binding.
pub type CoreMlAdapter = AcceleratorAdapter<CoreMl>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::{InferenceAdapter, ProbeResult};
    use crate::adapters::accelerator::MockAcceleratorBackend;
    use crate::config::{DeviceTier, RouterConfig};
    use crate::task::InferenceTask;

    fn high() -> RouterConfig {
        RouterConfig::default().with_device_tier(DeviceTier::High)
    }

    #[test]
    fn reports_coreml_kind() {
        let a = CoreMlAdapter::with_platform_override(
            high(),
            Box::new(MockAcceleratorBackend::full_deterministic("x")),
            true,
        );
        assert_eq!(a.kind(), AdapterKind::CoreMl);
        assert_eq!(a.kind().as_str(), "coreml");
    }

    #[test]
    fn available_on_apple_high_tier_with_backend() {
        let a = CoreMlAdapter::with_platform_override(
            high(),
            Box::new(MockAcceleratorBackend::full_deterministic("brief")),
            true,
        );
        assert_eq!(a.probe(), ProbeResult::Available);
        assert!(a.supports(InferenceTask::SynthSummary));
        assert_eq!(a.generate("synth_summary", "p", "").unwrap(), "brief");
    }

    #[test]
    fn unavailable_off_apple_platform() {
        let a = CoreMlAdapter::with_platform_override(
            high(),
            Box::new(MockAcceleratorBackend::full_deterministic("x")),
            false,
        );
        assert_eq!(a.probe(), ProbeResult::Unavailable);
    }
}
