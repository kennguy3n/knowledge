//! [`SynthesisPipeline`] — the synthesizer interface.
//!
//! This module ships:
//!
//! * The trait shape (`synthesize(window, inputs) -> SynthesisObject`).
//! * A [`NoOpSynthesizer`] test implementation that emits a
//!   well-formed [`SynthesisObject`] without invoking the SLM. Useful
//!   for end-to-end wiring tests in callers (channel recap path,
//!   CRDT merge path, etc.) before the on-device Bonsai-1.7B
//!   adapter lands.
//! * A [`LlamaCppSynthesizer`] that drives the
//!   `kennguy3n/llama.cpp@prism` loopback `llama-server` through the
//!   [`inference_router::InferenceRouter`] with a GBNF grammar
//!   constraining the SLM to emit [`SummaryBundle`] JSON.

use std::sync::Arc;

use uuid::Uuid;

use inference_router::{InferenceRouter, InferenceTask};

use crate::error::{PipelineError, Result};
use crate::object::{SynthesisObject, SynthesisObjectType};
use crate::schema::{ObservationRow, SummaryBundle};
use crate::window::SynthesisWindow;

/// Inputs to one synthesis run.
///
/// The `NoOpSynthesizer` only consumes the [`SynthesisInputs::recap_seed`]
/// field. The SLM-backed synthesizer will consume the
/// observation-row inputs (`observations`) and produce a real
/// [`SummaryBundle`].
#[derive(Debug, Default, Clone)]
pub struct SynthesisInputs {
    /// The structured-output records the SLM should aggregate (the
    /// observation rows in the window). Left empty for the no-op
    /// synthesizer.
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
    fn synthesize(&self,
        window: &SynthesisWindow,
        inputs: &SynthesisInputs,
    ) -> Result<SynthesisObject>;
}

/// No-op synthesizer used in tests and integration scaffolding.
///
/// Emits a [`SynthesisObject`] of type [`SynthesisObjectType::ChannelRecap`]
/// whose payload is a JSON-encoded [`SummaryBundle`] with the recap
/// seed copied verbatim and empty decision / question / task lists.
///
/// Gated behind `#[cfg(any(test, feature = "test-support"))]` so it
/// does not ship in default `cargo build` artifacts. The real
/// SLM-backed synthesizer lands when the on-device Bonsai-1.7B
/// adapter is wired through the inference router.
#[cfg(any(test, feature = "test-support"))]
#[derive(Debug, Default, Clone)]
pub struct NoOpSynthesizer {
    /// Object type to emit. Defaults to [`SynthesisObjectType::ChannelRecap`].
    pub object_type: SynthesisObjectType,
    /// Provenance reference to attach to the object. Defaults to a
    /// fresh `Uuid::nil()` so callers can spot the placeholder.
    pub provenance_ref: Uuid,
}

#[cfg(any(test, feature = "test-support"))]
impl NoOpSynthesizer {
    /// Construct a fresh no-op synthesizer.
    pub fn new() -> Self {
        Self::default()
    }
}

#[cfg(any(test, feature = "test-support"))]
impl SynthesisPipeline for NoOpSynthesizer {
    fn synthesize(&self,
        window: &SynthesisWindow,
        inputs: &SynthesisInputs,
    ) -> Result<SynthesisObject> {
        let bundle = SummaryBundle {
            recap: inputs.recap_seed.clone(),
            ..SummaryBundle::default()
        };
        let payload = serde_json::to_vec(&bundle)
            .map_err(|_| crate::error::PipelineError::Serialisation("SummaryBundle::to_vec"))?;
        Ok(SynthesisObject::new(window.scope_id,
            window.id,
            self.object_type,
            payload,
            self.provenance_ref,
        ))
    }
}

/// Real synthesizer that drives the on-device `llama-server` through
/// the [`InferenceRouter`].
///
/// The router automatically threads the GBNF grammar registered for
/// [`InferenceTask::SynthSummary`]
/// (`inference_router::task::GRAMMAR_SYNTH_SUMMARY`) into the
/// `llama-server` `/completion` call, so the model can only emit
/// JSON that round-trips through [`SummaryBundle`]'s `serde`
/// derive.
///
/// # All summary tiers share the [`InferenceTask::SynthSummary`] task
///
/// [`Self::with_object_type`] lets the caller tag the emitted
/// [`SynthesisObject`] as `ChannelRecap`, `EpisodicSummary`,
/// `DomainSummary`, or `TenantSummary`, but every tier dispatches
/// the **same** [`InferenceTask::SynthSummary`] under the hood — and
/// therefore the same prompt template and the same GBNF grammar.
/// The four tiers currently share one [`SummaryBundle`] output
/// shape, so a single task is the right design today; the
/// `object_type` field is purely an output-side annotation that
/// downstream consumers use to route the bundle into the
/// appropriate scope.
///
/// If a future tier needs a different prompt or grammar (e.g. a
/// tenant rollup that wants per-domain breakdowns) the right shape
/// is a new [`InferenceTask`] variant
/// (`SynthDomainSummary` / `SynthTenantSummary`) plus a per-tier
/// dispatch table here, **not** a runtime fork inside the prompt
/// template. Don't introduce that split until a concrete tier
/// actually diverges — `InferenceTask` is part of the cross-crate
/// contract.
///
/// On any router-level failure (no adapter available, adapter
/// crash, JSON parse error after the grammar constraint somehow
/// failed) we surface
/// [`crate::error::PipelineError::SynthesisFailed`]; the substrate
/// is expected to fall back to the deterministic
/// [`NoOpSynthesizer`] when this fires.
#[derive(Clone)]
pub struct LlamaCppSynthesizer {
    router: Arc<InferenceRouter>,
    object_type: SynthesisObjectType,
    provenance_ref: Uuid,
}

impl LlamaCppSynthesizer {
    /// Build a synthesizer wrapping the given router. The caller is
    /// responsible for calling [`InferenceRouter::bootstrap`]
    /// before the first synthesis run.
    pub fn new(router: Arc<InferenceRouter>) -> Self {
        Self {
            router,
            object_type: SynthesisObjectType::ChannelRecap,
            provenance_ref: Uuid::nil(),
        }
    }

    /// Override the [`SynthesisObjectType`] emitted by
    /// [`Self::synthesize`]. Useful when reusing the same router
    /// for episodic / domain / tenant rollups.
    pub fn with_object_type(mut self, object_type: SynthesisObjectType) -> Self {
        self.object_type = object_type;
        self
    }

    /// Override the provenance ref stamped on every emitted
    /// [`SynthesisObject`].
    pub fn with_provenance_ref(mut self, provenance_ref: Uuid) -> Self {
        self.provenance_ref = provenance_ref;
        self
    }

    /// Borrow the underlying router (mostly useful for tests).
    pub fn router(&self) -> &InferenceRouter {
        &self.router
    }

    /// Build the prompt body that gets substituted into the
    /// [`InferenceTask::SynthSummary`] prompt template's `{body}`
    /// placeholder.
    fn build_prompt(window: &SynthesisWindow, inputs: &SynthesisInputs) -> String {
        let template = InferenceTask::SynthSummary.prompt_template();
        let body = render_inputs(window, inputs);
        template.replace("{body}", &body)
    }
}

impl SynthesisPipeline for LlamaCppSynthesizer {
    fn synthesize(&self,
        window: &SynthesisWindow,
        inputs: &SynthesisInputs,
    ) -> Result<SynthesisObject> {
        let prompt = Self::build_prompt(window, inputs);
        let raw = self
            .router
            .dispatch(InferenceTask::SynthSummary, &prompt)
            .map_err(|e| PipelineError::SynthesisFailed(e.to_string()))?;

        // The grammar constrains output to `SummaryBundle` shape;
        // a parse error here means the adapter (or a misconfigured
        // grammar) broke its contract, so we surface it as a
        // synthesis failure rather than masking it.
        let bundle: SummaryBundle = serde_json::from_str(raw.trim()).map_err(|e| {
            PipelineError::SynthesisFailed(format!("SLM output did not parse as SummaryBundle: {e}; raw=`{raw}`"
            ))
        })?;

        let payload = serde_json::to_vec(&bundle)
            .map_err(|_| PipelineError::Serialisation("SummaryBundle::to_vec"))?;

        Ok(SynthesisObject::new(window.scope_id,
            window.id,
            self.object_type,
            payload,
            self.provenance_ref,
        ))
    }
}

/// Format `inputs` into a deterministic, line-oriented body that
/// substitutes into the SynthSummary prompt template. Each
/// observation becomes `- [kind] (importance) content`, sorted by
/// `kind` and then by `content` so identical inputs always produce
/// identical prompts (= deterministic cache keys for callers that
/// memoise the SLM call).
fn render_inputs(window: &SynthesisWindow, inputs: &SynthesisInputs) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    let _ = writeln!(&mut out,
        "Window: {} \u{2192} {}",
        window.window_start.to_rfc3339(),
        window.window_end.to_rfc3339()
    );

    if !inputs.recap_seed.is_empty() {
        let _ = writeln!(&mut out, "Recap seed: {}", inputs.recap_seed);
    }

    let mut rows: Vec<&ObservationRow> = inputs.observations.iter().collect();
    rows.sort_by(|a, b| {
        observation_kind_tag(a.kind)
            .cmp(observation_kind_tag(b.kind))
            .then_with(|| a.content.cmp(&b.content))
    });

    if rows.is_empty() {
        let _ = writeln!(&mut out, "(no structured observations recorded)");
    } else {
        let _ = writeln!(&mut out, "Observations:");
        for row in rows {
            let _ = writeln!(&mut out,
                "- [{}] ({}) {}",
                observation_kind_tag(row.kind),
                importance_class_tag(row.importance),
                row.content
            );
        }
    }

    out
}

/// Stable string tag for [`crate::schema::ObservationRowKind`].
/// Kept local to the synthesizer so we don't widen the schema
/// module's public surface.
const fn observation_kind_tag(kind: crate::schema::ObservationRowKind) -> &'static str {
    use crate::schema::ObservationRowKind as K;
    match kind {
        K::Entity => "entity",
        K::Fact => "fact",
        K::Task => "task",
        K::Decision => "decision",
        K::Claim => "claim",
        K::Question => "question",
    }
}

/// Stable string tag for [`crate::schema::ImportanceTagClass`].
const fn importance_class_tag(class: crate::schema::ImportanceTagClass) -> &'static str {
    use crate::schema::ImportanceTagClass as I;
    match class {
        I::Critical => "critical",
        I::Important => "important",
        I::Useful => "useful",
        I::Noise => "noise",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{ImportanceTagClass, ObservationRow, ObservationRowKind};
    use crate::window::SynthesisWindow;
    use chrono::{Duration, Utc};
    use evidence_store::ScopeId;
    use inference_router::adapter::{AdapterKind, InferenceAdapter, ProbeResult};
    use inference_router::adapters::llama_cpp::MockLlamaServerClient;
    use inference_router::{
        DeviceTier, FallbackAdapter, InferenceRouter, LlamaCppAdapter, RouterConfig,
    };
    use std::sync::Arc;

    fn fresh_window() -> SynthesisWindow {
        let scope = ScopeId::new_v4();
        let now = Utc::now();
        SynthesisWindow::new(scope, now - Duration::hours(1), now).unwrap()
    }

    fn observation(kind: ObservationRowKind, content: &str) -> ObservationRow {
        ObservationRow {
            kind,
            content: content.into(),
            importance: ImportanceTagClass::Important,
            confidence: 0.9,
        }
    }

    fn build_router_with_llama(client: MockLlamaServerClient) -> Arc<InferenceRouter> {
        // Tier `High` so `LlamaCppAdapter` accepts synthesis tasks.
        let cfg = RouterConfig::default().with_device_tier(DeviceTier::High);
        let llama = Box::new(LlamaCppAdapter::new(cfg.clone(), Box::new(client)));
        let fallback = Box::new(FallbackAdapter::default());
        let adapters: Vec<Box<dyn InferenceAdapter>> = vec![llama, fallback];
        let router = Arc::new(InferenceRouter::new(cfg, adapters));
        router.bootstrap();
        router
    }

    #[test]
    fn no_op_emits_well_formed_summary_payload() {
        let synth = NoOpSynthesizer::new();
        let window = fresh_window();
        let object = synth
            .synthesize(&window, &SynthesisInputs::from_recap("a productive hour"))
            .unwrap();
        assert_eq!(object.window_id, window.id);
        let bundle: SummaryBundle = serde_json::from_slice(&object.payload).unwrap();
        assert_eq!(bundle.recap, "a productive hour");
    }

    #[test]
    fn llama_cpp_synthesizer_round_trips_summary_bundle() {
        let response = r#"{"recap":"Picked vendor X.","decisions":["chose vendor X"],"open_questions":[],"active_tasks":["sign by Friday"]}"#;
        let router = build_router_with_llama(MockLlamaServerClient::ok(response));
        let synth = LlamaCppSynthesizer::new(router);
        let window = fresh_window();
        let inputs = SynthesisInputs {
            observations: vec![
                observation(ObservationRowKind::Decision, "chose vendor X"),
                observation(ObservationRowKind::Task, "sign by Friday"),
            ],
            recap_seed: "vendor selection".into(),
        };
        let object = synth.synthesize(&window, &inputs).expect("synth ok");
        assert_eq!(object.scope_id, window.scope_id);
        assert_eq!(object.window_id, window.id);
        assert_eq!(object.object_type, SynthesisObjectType::ChannelRecap);
        let bundle: SummaryBundle = serde_json::from_slice(&object.payload).unwrap();
        assert_eq!(bundle.recap, "Picked vendor X.");
        assert_eq!(bundle.decisions, vec!["chose vendor X".to_string()]);
        assert_eq!(bundle.active_tasks, vec!["sign by Friday".to_string()]);
        assert!(bundle.open_questions.is_empty());
    }

    #[test]
    fn llama_cpp_synthesizer_propagates_router_unavailable_as_synthesis_failed() {
        // No reachable adapter -> dispatch returns Unavailable ->
        // synthesizer surfaces SynthesisFailed.
        let router = build_router_with_llama(MockLlamaServerClient::unreachable());
        let synth = LlamaCppSynthesizer::new(router);
        let err = synth
            .synthesize(&fresh_window(), &SynthesisInputs::default())
            .unwrap_err();
        assert!(matches!(err, PipelineError::SynthesisFailed(_)),
            "expected SynthesisFailed, got {err:?}"
        );
    }

    #[test]
    fn llama_cpp_synthesizer_surfaces_parse_errors() {
        // The grammar would normally prevent malformed output, but we
        // assert the failure path is loud rather than silent in case
        // the adapter ever returns something off-spec.
        let router = build_router_with_llama(MockLlamaServerClient::ok("not json at all"));
        let synth = LlamaCppSynthesizer::new(router);
        let err = synth
            .synthesize(&fresh_window(), &SynthesisInputs::default())
            .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("did not parse as SummaryBundle"),
            "expected parse failure, got: {msg}"
        );
    }

    #[test]
    fn llama_cpp_synthesizer_respects_object_type_override() {
        let response = r#"{"recap":"r","decisions":[],"open_questions":[],"active_tasks":[]}"#;
        let router = build_router_with_llama(MockLlamaServerClient::ok(response));
        let synth =
            LlamaCppSynthesizer::new(router).with_object_type(SynthesisObjectType::DomainSummary);
        let object = synth
            .synthesize(&fresh_window(), &SynthesisInputs::default())
            .unwrap();
        assert_eq!(object.object_type, SynthesisObjectType::DomainSummary);
    }

    #[test]
    fn prompt_includes_observations_in_deterministic_order() {
        // Same inputs in different insertion order should produce
        // identical prompt bodies. Locks in the sort behaviour for
        // any caller that memoises the SLM call.
        let a = SynthesisInputs {
            observations: vec![
                observation(ObservationRowKind::Task, "sign vendor doc"),
                observation(ObservationRowKind::Decision, "chose vendor X"),
            ],
            recap_seed: "vendor selection".into(),
        };
        let b = SynthesisInputs {
            observations: vec![
                observation(ObservationRowKind::Decision, "chose vendor X"),
                observation(ObservationRowKind::Task, "sign vendor doc"),
            ],
            recap_seed: "vendor selection".into(),
        };
        let window = fresh_window();
        assert_eq!(LlamaCppSynthesizer::build_prompt(&window, &a),
            LlamaCppSynthesizer::build_prompt(&window, &b)
        );
    }

    #[test]
    fn router_is_borrowable_after_construction() {
        // Smoke test the helper that lets tests / shells inspect
        // the underlying router after construction.
        let router = build_router_with_llama(MockLlamaServerClient::ok("x"));
        let synth = LlamaCppSynthesizer::new(Arc::clone(&router));
        assert_eq!(synth.router().config().device_tier, DeviceTier::High);
        // Adapter kinds match what we passed in.
        let probed: Vec<AdapterKind> = router.bootstrap().into_iter().map(|(k, _)| k).collect();
        assert!(probed.contains(&AdapterKind::LlamaCpp));
    }

    #[test]
    fn bootstrap_required_before_dispatch_surfaces_as_synthesis_failed() {
        let cfg = RouterConfig::default().with_device_tier(DeviceTier::High);
        let llama = Box::new(LlamaCppAdapter::new(cfg.clone(),
            Box::new(MockLlamaServerClient::ok("x")),
        ));
        let adapters: Vec<Box<dyn InferenceAdapter>> = vec![llama];
        // No bootstrap call -> dispatch returns NotProbed.
        let router = Arc::new(InferenceRouter::new(cfg, adapters));
        let synth = LlamaCppSynthesizer::new(router);
        let err = synth
            .synthesize(&fresh_window(), &SynthesisInputs::default())
            .unwrap_err();
        assert!(matches!(err, PipelineError::SynthesisFailed(s) if s.contains("not been probed")));
    }

    #[test]
    fn unused_probe_result_variant_is_referenced() {
        // Touch the import so unused-import lint stays clean if the
        // tests above ever stop covering both arms.
        let _ = ProbeResult::Available;
        let _ = ProbeResult::Unavailable;
    }
}
