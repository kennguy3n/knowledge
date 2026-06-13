//! **ONNX Runtime Mobile** inference adapter.
//!
//! Routes SLM work to ONNX Runtime Mobile configured with an NPU
//! execution provider — NNAPI or QNN (Qualcomm Hexagon) on Android, the
//! Core ML EP on iOS/macOS — falling back to the XNNPACK/CPU EP when no
//! NPU provider initialises. The ONNX Runtime session lives in the
//! platform shell (the Android JNI layer / desktop sidecar), which
//! selects and orders execution providers and owns the loaded `.onnx`
//! graph; this crate holds the routing seam — a host-injected
//! [`AcceleratorBackend`](crate::adapters::accelerator::AcceleratorBackend).
//! See [`crate::adapters::accelerator`] for the shared dispatch,
//! capability detection, and determinism-contract logic.
//!
//! Compiled only under the `onnx-runtime` feature.

use crate::adapter::AdapterKind;
use crate::adapters::accelerator::{AcceleratorAdapter, AcceleratorClass};

/// Marker type identifying the ONNX Runtime Mobile accelerator.
pub struct OnnxRuntime;

impl AcceleratorClass for OnnxRuntime {
    const KIND: AdapterKind = AdapterKind::OnnxRuntime;

    /// ONNX Runtime is cross-platform, so the static compile-target gate
    /// is permissive: whether a usable NPU execution provider actually
    /// initialised on this device is a dynamic decision the injected
    /// backend reports via
    /// [`AcceleratorBackend::capabilities`](crate::adapters::accelerator::AcceleratorBackend::capabilities)
    /// (`present`). This keeps the adapter usable on Android/iOS/desktop
    /// while still skipping the slot when no NPU provider is live.
    fn platform_supported() -> bool {
        true
    }
}

/// ONNX Runtime Mobile adapter. A concrete
/// [`AcceleratorAdapter`](crate::adapters::accelerator::AcceleratorAdapter)
/// specialised for the [`OnnxRuntime`] class.
///
/// Construct with
/// [`OnnxRuntimeAdapter::new(config, backend)`](crate::adapters::accelerator::AcceleratorAdapter::new),
/// where `backend` is the host's ONNX Runtime session binding.
pub type OnnxRuntimeAdapter = AcceleratorAdapter<OnnxRuntime>;

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
    fn reports_onnx_runtime_kind() {
        let a = OnnxRuntimeAdapter::new(
            high(),
            Box::new(MockAcceleratorBackend::full_deterministic("x")),
        );
        assert_eq!(a.kind(), AdapterKind::OnnxRuntime);
        assert_eq!(a.kind().as_str(), "onnx_runtime");
    }

    #[test]
    fn unavailable_when_no_npu_provider_live() {
        // Platform is permissive, but the backend reports no NPU EP
        // initialised → the router skips the slot.
        let a = OnnxRuntimeAdapter::new(high(), Box::new(MockAcceleratorBackend::unavailable()));
        assert_eq!(a.probe(), ProbeResult::Unavailable);
    }

    #[test]
    fn nondeterministic_npu_declines_synthesis_but_serves_classification() {
        // NNAPI/QNN kernels are commonly non-reproducible: synthesis
        // falls through to a deterministic SLM, classification stays on
        // the NPU.
        let a = OnnxRuntimeAdapter::new(
            high(),
            Box::new(MockAcceleratorBackend::nondeterministic("x")),
        );
        a.probe();
        assert!(!a.supports(InferenceTask::SynthSummary));
        assert!(a.supports(InferenceTask::ExtractEntities));
    }
}
