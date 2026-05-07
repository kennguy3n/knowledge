//! [`SynthesisPipeline`] — the synthesizer interface.
//!
//! Phase 2 ships:
//!
//! * The trait shape (`synthesize(window, inputs) -> SynthesisObject`).
//! * A [`NoOpSynthesizer`] test implementation that emits a
//!   well-formed [`SynthesisObject`] without invoking the SLM. Useful
//!   for end-to-end wiring tests in callers (channel recap path,
//!   CRDT merge path, etc.) before the on-device Bonsai-1.7B
//!   adapter lands.
//!
//! The on-device implementation that talks to the
//! `kennguy3n/llama.cpp@prism` `llama-server` (Bonsai-1.7B,
//! GBNF-constrained) lands when the SLM adapters are wired through
//! the inference router (`ARCHITECTURE.md` §3.2).

use uuid::Uuid;

use crate::error::Result;
use crate::object::{SynthesisObject, SynthesisObjectType};
use crate::schema::SummaryBundle;
use crate::window::SynthesisWindow;

/// Inputs to one synthesis run.
///
/// Phase 2's `NoOpSynthesizer` only consumes the [`SynthesisInputs::recap_seed`]
/// field. The SLM-backed synthesizer in later phases will consume the
/// observation-row inputs (`observations`) and produce a real
/// [`SummaryBundle`].
#[derive(Debug, Default, Clone)]
pub struct SynthesisInputs {
    /// The structured-output records the SLM should aggregate (the
    /// observation rows in the window). Phase 2 leaves this empty for
    /// the no-op synthesizer.
    pub observations: Vec<crate::schema::ObservationRow>,
    /// Seed text for the recap line. Useful for tests where the
    /// caller wants a deterministic synthesis output without an SLM.
    pub recap_seed: String,
}

impl SynthesisInputs {
    /// Convenience: build inputs whose only signal is a recap seed.
    pub fn from_recap(recap: impl Into<String>) -> Self {
        Self {
            recap_seed: recap.into(),
            observations: Vec::new(),
        }
    }
}

/// Synthesizer interface.
pub trait SynthesisPipeline {
    /// Synthesise an object for `window` from `inputs`. Returns the
    /// freshly-built [`SynthesisObject`] — the caller is responsible
    /// for publishing it via [`crate::publish::publish_synthesis_object`].
    fn synthesize(
        &self,
        window: &SynthesisWindow,
        inputs: &SynthesisInputs,
    ) -> Result<SynthesisObject>;
}

/// No-op synthesizer used in tests and integration scaffolding.
///
/// Emits a [`SynthesisObject`] of type [`SynthesisObjectType::ChannelRecap`]
/// whose payload is a JSON-encoded [`SummaryBundle`] with the recap
/// seed copied verbatim and empty decision / question / task lists.
#[derive(Debug, Default, Clone)]
pub struct NoOpSynthesizer {
    /// Object type to emit. Defaults to [`SynthesisObjectType::ChannelRecap`].
    pub object_type: SynthesisObjectType,
    /// Provenance reference to attach to the object. Defaults to a
    /// fresh `Uuid::nil()` so callers can spot the placeholder.
    pub provenance_ref: Uuid,
}

impl NoOpSynthesizer {
    /// Construct a fresh no-op synthesizer.
    pub fn new() -> Self {
        Self::default()
    }
}

impl SynthesisPipeline for NoOpSynthesizer {
    fn synthesize(
        &self,
        window: &SynthesisWindow,
        inputs: &SynthesisInputs,
    ) -> Result<SynthesisObject> {
        let bundle = SummaryBundle {
            recap: inputs.recap_seed.clone(),
            ..SummaryBundle::default()
        };
        let payload = serde_json::to_vec(&bundle)
            .map_err(|_| crate::error::PipelineError::Serialisation("SummaryBundle::to_vec"))?;
        Ok(SynthesisObject::new(
            window.scope_id,
            window.id,
            self.object_type,
            payload,
            self.provenance_ref,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::window::SynthesisWindow;
    use chrono::{Duration, Utc};
    use evidence_store::ScopeId;

    #[test]
    fn no_op_emits_well_formed_summary_payload() {
        let scope = ScopeId::new_v4();
        let now = Utc::now();
        let window = SynthesisWindow::new(scope, now - Duration::hours(1), now).unwrap();
        let synth = NoOpSynthesizer::new();
        let object = synth
            .synthesize(&window, &SynthesisInputs::from_recap("a productive hour"))
            .unwrap();
        assert_eq!(object.scope_id, scope);
        assert_eq!(object.window_id, window.id);
        let bundle: SummaryBundle = serde_json::from_slice(&object.payload).unwrap();
        assert_eq!(bundle.recap, "a productive hour");
    }
}
