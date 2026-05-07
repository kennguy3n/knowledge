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

/// Server-side synthesis engine. Phase 3 ships
/// [`crate::ManagedEndpointSynthesizer`] as a stub implementation.
pub trait SynthesisEngine {
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
    fn synthesize_domain(
        &self,
        windows: &mut SynthesisWindowManager,
        handle: TieredWindowHandle,
        input: DomainSynthesisInput,
    ) -> Result<DomainSynthesisResult>;

    /// Synthesise a [`synthesis_pipeline::SynthesisObjectType::TenantSummary`]
    /// for `handle` from `input`.
    fn synthesize_tenant(
        &self,
        windows: &mut SynthesisWindowManager,
        handle: TieredWindowHandle,
        input: TenantSynthesisInput,
    ) -> Result<TenantSynthesisResult>;
}
