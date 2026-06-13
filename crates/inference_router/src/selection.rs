//! Device-tier → adapter selection ordering.
//!
//! The [`crate::InferenceRouter`] dispatches to the first adapter in its
//! list that is both available and supports the task, so the *order* of
//! that list is the policy that decides which backend wins when several
//! could serve a task. This module is the single, unit-tested source of
//! truth for that order: given a [`RouterConfig`] (device tier +
//! accelerator preference) and which on-device accelerators are
//! actually present, [`ordered_adapter_kinds`] returns the priority list
//! the host should build its adapter stack in.
//!
//! Keeping the policy here (rather than inline in the platform shell's
//! stack builder) means the priority — and the fallback behaviour when
//! an accelerator is absent — is testable without constructing real
//! adapters or native backends. The runtime still applies the per-adapter
//! `probe()` / `supports()` gates on top of this order; this function
//! only expresses *preference*, never availability.

use crate::adapter::AdapterKind;
use crate::config::{DeviceTier, RouterConfig};

/// Which on-device NPU/accelerator backends are present on this device.
///
/// Supplied by the platform shell after it constructs (and probes) its
/// accelerator backends. Both default to `false` (no accelerator), so a
/// host that compiled without the `coreml` / `onnx-runtime` features —
/// or that runs on hardware without an NPU — gets the classic
/// `MLX → llama.cpp → managed-cloud → fallback` order.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AcceleratorAvailability {
    /// Apple Core ML / ANE backend is present and ready.
    pub coreml_ane: bool,
    /// ONNX Runtime Mobile with a live NPU execution provider is present.
    pub onnx_runtime: bool,
}

impl AcceleratorAvailability {
    /// No accelerators present — the classic SLM-only stack.
    pub const fn none() -> Self {
        Self {
            coreml_ane: false,
            onnx_runtime: false,
        }
    }

    /// `true` when at least one accelerator is present.
    pub const fn any(self) -> bool {
        self.coreml_ane || self.onnx_runtime
    }
}

/// The ordered list of adapter kinds the host should try, highest
/// priority first, for the given config and accelerator availability.
///
/// Policy:
/// * **Low tier** runs no local model (insufficient RAM), so only the
///   remote managed-cloud synthesis path and the encoder-only fallback
///   appear — accelerators are omitted even when present.
/// * **Medium / High tier** include the present accelerators alongside
///   the MLX / llama.cpp SLM adapters. When
///   [`RouterConfig::prefer_accelerator`] is set (the default) the
///   accelerators are ranked *ahead* of MLX / llama.cpp to minimise
///   on-device latency and battery cost; otherwise they trail the SLM
///   path (useful while validating a new accelerator backend). The
///   managed-cloud and fallback adapters always close the list.
///
/// When an accelerator is not present it is simply omitted, so the list
/// degrades gracefully to the SLM/fallback order — this is the fallback
/// behaviour exercised by the unit tests.
#[must_use]
pub fn ordered_adapter_kinds(
    config: &RouterConfig,
    accel: AcceleratorAvailability,
) -> Vec<AdapterKind> {
    let mut accelerators = Vec::with_capacity(2);
    if accel.coreml_ane {
        accelerators.push(AdapterKind::CoreMl);
    }
    if accel.onnx_runtime {
        accelerators.push(AdapterKind::OnnxRuntime);
    }

    let mut order = Vec::with_capacity(6);
    match config.device_tier {
        DeviceTier::Low => {
            // No on-device model: remote synthesis + encoder fallback only.
        }
        DeviceTier::Medium | DeviceTier::High => {
            let slm = [AdapterKind::Mlx, AdapterKind::LlamaCpp];
            if config.prefer_accelerator {
                order.extend(accelerators);
                order.extend(slm);
            } else {
                order.extend(slm);
                order.extend(accelerators);
            }
        }
    }
    // Remote synthesis is tier-independent (the compute is off-device),
    // and the encoder-only fallback is always last.
    order.push(AdapterKind::ManagedCloud);
    order.push(AdapterKind::Fallback);
    order
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(tier: DeviceTier) -> RouterConfig {
        RouterConfig::default().with_device_tier(tier)
    }

    #[test]
    fn high_tier_prefers_accelerators_ahead_of_slm() {
        let order = ordered_adapter_kinds(
            &cfg(DeviceTier::High),
            AcceleratorAvailability {
                coreml_ane: true,
                onnx_runtime: true,
            },
        );
        assert_eq!(
            order,
            vec![
                AdapterKind::CoreMl,
                AdapterKind::OnnxRuntime,
                AdapterKind::Mlx,
                AdapterKind::LlamaCpp,
                AdapterKind::ManagedCloud,
                AdapterKind::Fallback,
            ]
        );
    }

    #[test]
    fn prefer_accelerator_false_ranks_slm_first() {
        let config = cfg(DeviceTier::High).with_prefer_accelerator(false);
        let order = ordered_adapter_kinds(
            &config,
            AcceleratorAvailability {
                coreml_ane: true,
                onnx_runtime: false,
            },
        );
        assert_eq!(
            order,
            vec![
                AdapterKind::Mlx,
                AdapterKind::LlamaCpp,
                AdapterKind::CoreMl,
                AdapterKind::ManagedCloud,
                AdapterKind::Fallback,
            ]
        );
    }

    #[test]
    fn no_accelerator_degrades_to_classic_stack() {
        // Fallback behaviour: with no NPU present the order is exactly
        // the pre-existing MLX → llama.cpp → managed → fallback stack.
        let order = ordered_adapter_kinds(&cfg(DeviceTier::High), AcceleratorAvailability::none());
        assert_eq!(
            order,
            vec![
                AdapterKind::Mlx,
                AdapterKind::LlamaCpp,
                AdapterKind::ManagedCloud,
                AdapterKind::Fallback,
            ]
        );
    }

    #[test]
    fn only_present_accelerators_are_listed() {
        let order = ordered_adapter_kinds(
            &cfg(DeviceTier::Medium),
            AcceleratorAvailability {
                coreml_ane: false,
                onnx_runtime: true,
            },
        );
        assert_eq!(order.first(), Some(&AdapterKind::OnnxRuntime));
        assert!(!order.contains(&AdapterKind::CoreMl));
    }

    #[test]
    fn low_tier_omits_local_adapters() {
        let order = ordered_adapter_kinds(
            &cfg(DeviceTier::Low),
            AcceleratorAvailability {
                coreml_ane: true,
                onnx_runtime: true,
            },
        );
        assert_eq!(
            order,
            vec![AdapterKind::ManagedCloud, AdapterKind::Fallback]
        );
    }

    #[test]
    fn availability_any_reports_presence() {
        assert!(!AcceleratorAvailability::none().any());
        assert!(AcceleratorAvailability {
            coreml_ane: true,
            onnx_runtime: false
        }
        .any());
    }
}
