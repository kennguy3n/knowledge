//! Stub managed-endpoint synthesizer.
//!
//! Deterministically concatenates the input
//! payloads (channel-output bytes for domain synthesis, domain-output
//! and approved-doc bytes for tenant synthesis) into a single payload
//! prefixed with a hierarchy-tier marker. Useful for end-to-end tests
//! and for the server-skeleton wiring; the real managed-AI
//! endpoint adapter lands when the SLM gateway is wired through.

use uuid::Uuid;

use synthesis_pipeline::{
    build_domain_summary_object, build_tenant_summary_object, DomainSynthesisInput,
    HierarchyEnforcedWindowManager, PipelineError, SynthesisWindowManager, TenantSynthesisInput,
    TieredWindowHandle,
};

use crate::engine::{DomainSynthesisResult, SynthesisEngine, TenantSynthesisResult};
use crate::error::{EngineError, Result};

/// Map a [`PipelineError`] surfaced by hierarchy validation onto an
/// [`EngineError`]. Only [`PipelineError::HierarchyViolation`] becomes
/// [`EngineError::Hierarchy`]; every other variant (e.g.
/// `WindowNotFound`, `InvalidWindowTransition`) is preserved as
/// [`EngineError::Pipeline`] so downstream error matching stays
/// consistent with the rest of the engine.
fn map_validation_error(e: PipelineError) -> EngineError {
    match e {
        PipelineError::HierarchyViolation(msg) => EngineError::Hierarchy(msg),
        other => EngineError::Pipeline(other),
    }
}

/// Stub managed-endpoint synthesizer.
#[derive(Debug, Clone, Default)]
pub struct ManagedEndpointSynthesizer {
    /// Provenance reference attached to every emitted synthesis
    /// object. Defaults to a fresh `Uuid::nil()` so callers can
    /// spot the placeholder.
    pub provenance_ref: Uuid,
}

impl ManagedEndpointSynthesizer {
    /// Construct a fresh stub synthesizer.
    pub fn new() -> Self {
        Self::default()
    }
}

impl SynthesisEngine for ManagedEndpointSynthesizer {
    fn synthesize_domain(&self,
        windows: &mut SynthesisWindowManager,
        handle: TieredWindowHandle,
        input: DomainSynthesisInput,
    ) -> Result<DomainSynthesisResult> {
        windows
            .validate_domain_input(&handle, &input)
            .map_err(map_validation_error)?;
        windows.mark_in_progress(handle.window_id)?;

        // Deterministic stub payload: `domain:` prefix + concatenated
        // channel-output bytes, separated by `\n`.
        let mut payload = b"domain:".to_vec();
        for (i, o) in input.channel_outputs.iter().enumerate() {
            if i > 0 {
                payload.push(b'\n');
            }
            payload.extend_from_slice(&o.object().payload);
        }

        let object = build_domain_summary_object(input.domain_scope,
            handle.window_id,
            payload,
            self.provenance_ref,
        );
        windows.mark_complete(handle.window_id)?;
        Ok(DomainSynthesisResult { object })
    }

    fn synthesize_tenant(&self,
        windows: &mut SynthesisWindowManager,
        handle: TieredWindowHandle,
        input: TenantSynthesisInput,
    ) -> Result<TenantSynthesisResult> {
        windows
            .validate_tenant_input(&handle, &input)
            .map_err(map_validation_error)?;
        windows.mark_in_progress(handle.window_id)?;

        let mut payload = b"tenant:".to_vec();
        for (i, o) in input.domain_outputs.iter().enumerate() {
            if i > 0 {
                payload.push(b'\n');
            }
            payload.extend_from_slice(&o.object().payload);
        }
        for d in input.approved_documents.iter() {
            payload.push(b'\n');
            payload.extend_from_slice(b"doc:");
            payload.extend_from_slice(&d.payload);
        }

        let object = build_tenant_summary_object(input.tenant_scope,
            handle.window_id,
            payload,
            self.provenance_ref,
        );
        windows.mark_complete(handle.window_id)?;
        Ok(TenantSynthesisResult { object })
    }
}
