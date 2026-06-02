//! [`SynthesisEngine`] trait.

use serde::{Deserialize, Serialize};

use synthesis_pipeline::{
    DomainSynthesisInput, SynthesisObject, SynthesisWindowManager, TenantSynthesisInput,
    TieredWindowHandle,
};

use crate::error::Result;

/// Output of [`SynthesisEngine::synthesize_domain`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DomainSynthesisResult {
    /// The synthesised [`SynthesisObject`] of type
    /// [`synthesis_pipeline::SynthesisObjectType::DomainSummary`].
    pub object: SynthesisObject,
}

/// Output of [`SynthesisEngine::synthesize_tenant`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TenantSynthesisResult {
    /// The synthesised [`SynthesisObject`] of type
    /// [`synthesis_pipeline::SynthesisObjectType::TenantSummary`].
    pub object: SynthesisObject,
}

/// Server-side synthesis engine.
///
/// Ships three production-grade implementations:
///
/// * [`crate::ManagedEndpointSynthesizer`] — deterministic test
///   stub used by end-to-end tests.
/// * [`crate::HttpManagedEndpointSynthesizer`] — production
///   synthesizer that POSTs through an [`crate::HttpClient`]
///   (real `BlockingHttpClientAdapter` behind the `http-client`
///   feature; `MockHttpClient` in tests).
/// * [`crate::tee_worker::TeeWorker`] — TEE-attested wrapper that
///   adds scope-binding enforcement around the leaf synthesizer.
///
/// The `Send + Sync` supertraits are required so the FFI substrate
/// can store the engine behind `Arc<dyn SynthesisEngine>` on the
/// per-handle [`crate::engine`] slot. The substrate's three-phase
/// locking discipline (gather-locked → dispatch-unlocked →
/// apply-locked) clones the `Arc` out of the runtime mutex before
/// the (potentially multi-second) HTTP dispatch so other FFI calls
/// on the same handle stay unblocked.
pub trait SynthesisEngine: Send + Sync {
    /// Synthesise a [`synthesis_pipeline::SynthesisObjectType::DomainSummary`]
    /// for `handle` from `input`.
    ///
    /// # Errors
    ///
    /// * [`crate::error::EngineError::Pipeline`] if the underlying
    ///   window manager rejects `handle` (wrong tier / missing
    ///   window).
    /// * [`crate::error::EngineError::Hierarchy`] if `input` does not
    ///   target the same scope as `handle`.
    fn synthesize_domain(&self,
        windows: &mut SynthesisWindowManager,
        handle: TieredWindowHandle,
        input: DomainSynthesisInput,
    ) -> Result<DomainSynthesisResult>;

    /// Synthesise a [`synthesis_pipeline::SynthesisObjectType::TenantSummary`]
    /// for `handle` from `input`.
    fn synthesize_tenant(&self,
        windows: &mut SynthesisWindowManager,
        handle: TieredWindowHandle,
        input: TenantSynthesisInput,
    ) -> Result<TenantSynthesisResult>;
}
