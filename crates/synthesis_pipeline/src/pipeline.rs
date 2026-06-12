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

use inference_router::{InferenceRouter, InferenceTask, SamplingConfig};

use crate::error::{PipelineError, Result};
use crate::metrics::{SynthesisMetrics, SynthesisMetricsSnapshot};
use crate::object::{SynthesisObject, SynthesisObjectType};
use crate::quality::{salient_terms_from_texts, verify_and_retry, Attempt};
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
    metrics: Arc<SynthesisMetrics>,
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
            metrics: SynthesisMetrics::new(),
        }
    }

    /// Share an externally-owned [`SynthesisMetrics`] so several
    /// synthesizers (e.g. one per object tier) fold their quality
    /// counters into the same totals the host exposes. Without this each
    /// synthesizer keeps its own counters (created in [`Self::new`]).
    #[must_use]
    pub fn with_metrics(mut self, metrics: Arc<SynthesisMetrics>) -> Self {
        self.metrics = metrics;
        self
    }

    /// Borrow the shared metrics handle (e.g. to register it with a host
    /// exposition surface before the first synthesis run).
    #[must_use]
    pub fn metrics(&self) -> &Arc<SynthesisMetrics> {
        &self.metrics
    }

    /// Point-in-time snapshot of the synthesis-quality counters for the
    /// metrics exposition: `synthesis_retry_total`,
    /// `synthesis_retry_failed_total`, `synthesis_lowquality_total`,
    /// `synthesis_truncated_total`, `synthesis_exemplar_leaks_stripped_total`,
    /// and the recap-length signal (`recap_length_sum` / `recap_length_count`).
    #[must_use]
    pub fn metrics_snapshot(&self) -> SynthesisMetricsSnapshot {
        self.metrics.snapshot()
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

    /// Run one SLM attempt: dispatch with the given per-call `sampling`,
    /// parse the bundle, and report whether the token cap truncated the
    /// output (strict parse failed but the salvage parser recovered it).
    ///
    /// A parse failure that even the salvage parser cannot recover is a
    /// genuinely unusable output, surfaced as
    /// [`PipelineError::SynthesisFailed`] so the substrate fails closed
    /// onto the deterministic [`NoOpSynthesizer`].
    fn run_attempt(&self, prompt: &str, sampling: &SamplingConfig) -> Result<Attempt> {
        let raw = self
            .router
            .dispatch_with_sampling(InferenceTask::SynthSummary, prompt, sampling)
            .map_err(|e| PipelineError::SynthesisFailed(e.to_string()))?;

        // The grammar constrains output to `SummaryBundle` shape but not
        // its length: a token-capped SLM can be cut off mid-string.
        // `from_slm_str_salvaged` does the strict parse once and reports
        // whether a truncated prefix had to be salvaged — so we get the
        // truncation signal without re-running the strict parse ourselves.
        let (bundle, truncated) = SummaryBundle::from_slm_str_salvaged(&raw).map_err(|e| {
            PipelineError::SynthesisFailed(format!(
                "SLM output did not parse as SummaryBundle: {e}; raw=`{raw}`"
            ))
        })?;
        Ok(Attempt { bundle, truncated })
    }
}

impl SynthesisPipeline for LlamaCppSynthesizer {
    fn synthesize(
        &self,
        window: &SynthesisWindow,
        inputs: &SynthesisInputs,
    ) -> Result<SynthesisObject> {
        let prompt = Self::build_prompt(window, inputs);
        let base = self.router.config().sampling;
        let salient = salient_terms_from_texts(
            inputs
                .observations
                .iter()
                .map(|row| row.content.as_str())
                .chain(std::iter::once(inputs.recap_seed.as_str())),
        );

        // Deterministic verify-and-retry policy, shared with the FFI
        // on-device path (see `synthesis_pipeline::quality`): first
        // attempt at an adaptive `n_predict` budget that scales with the
        // observation-row count but stays bounded under the synthesis
        // deadline; if the recap trips a quality flag, retry ONCE with a
        // larger budget + fact-only suffix and keep the better-scoring
        // bundle. The closure owns transport (greedy + fixed-seed preset
        // threaded per call); the policy owns the decision.
        let verified = verify_and_retry(
            &prompt,
            inputs.observations.len(),
            &salient,
            |attempt_prompt, n_predict| {
                self.run_attempt(attempt_prompt, &base.with_n_predict(n_predict))
            },
        )?;

        if verified.low_quality {
            self.metrics.incr_lowquality();
        }
        if verified.retried {
            self.metrics.incr_retry();
        }
        if verified.retry_failed {
            self.metrics.incr_retry_failed();
        }
        // `verified.exemplar_leaks_stripped` records any leaked-exemplar
        // entries the quality gate scrubbed before persistence. This pure
        // library crate carries no logging facade (unlike the FFI path,
        // which emits a `tracing::warn!`), so instead of logging we fold the
        // count into `synthesis_exemplar_leaks_stripped_total` — the server
        // path's metric-shaped equivalent of that warning, scraped via
        // `metrics_snapshot`. A no-op for the common (clean) case.
        self.metrics
            .add_exemplar_leaks_stripped(usize::from(verified.exemplar_leaks_stripped));
        for _ in 0..verified.truncated_attempts {
            self.metrics.incr_truncated();
        }
        self.metrics.observe_recap_length(verified.recap_chars);

        let payload = serde_json::to_vec(&verified.bundle)
            .map_err(|_| PipelineError::Serialisation("SummaryBundle::to_vec"))?;

        Ok(SynthesisObject::new(
            window.scope_id,
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
    let _ = writeln!(
        &mut out,
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
            let _ = writeln!(
                &mut out,
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
        assert!(
            matches!(err, PipelineError::SynthesisFailed(_)),
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
        assert!(
            msg.contains("did not parse as SummaryBundle"),
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
        assert_eq!(
            LlamaCppSynthesizer::build_prompt(&window, &a),
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
        let llama = Box::new(LlamaCppAdapter::new(
            cfg.clone(),
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

    use inference_router::adapters::llama_cpp::LlamaServerClient;
    use inference_router::SamplingConfig;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// Test client that replays a fixed sequence of responses (one per
    /// call, repeating the last once exhausted) and records the
    /// `n_predict` budget every call received, so a test can assert both
    /// the verify-and-retry behaviour and the adaptive-budget threading.
    struct SequencedClient {
        responses: Mutex<VecDeque<String>>,
        budgets: Arc<Mutex<Vec<u32>>>,
    }

    impl SequencedClient {
        fn new(responses: &[&str], budgets: Arc<Mutex<Vec<u32>>>) -> Self {
            Self {
                responses: Mutex::new(responses.iter().map(|s| (*s).to_string()).collect()),
                budgets,
            }
        }

        fn next_response(&self) -> String {
            let mut q = self.responses.lock().expect("responses");
            if q.len() > 1 {
                q.pop_front().expect("non-empty")
            } else {
                q.front().cloned().unwrap_or_default()
            }
        }
    }

    impl LlamaServerClient for SequencedClient {
        fn ping(&self) -> bool {
            true
        }
        fn complete(&self, _prompt: &str, _grammar: &str) -> Result<String, String> {
            Ok(self.next_response())
        }
        fn complete_with_sampling(
            &self,
            _prompt: &str,
            _grammar: &str,
            sampling: &SamplingConfig,
        ) -> Result<String, String> {
            self.budgets
                .lock()
                .expect("budgets")
                .push(sampling.n_predict);
            Ok(self.next_response())
        }
    }

    fn build_router_with_client(client: SequencedClient) -> Arc<InferenceRouter> {
        let cfg = RouterConfig::default().with_device_tier(DeviceTier::High);
        let llama = Box::new(LlamaCppAdapter::new(cfg.clone(), Box::new(client)));
        let fallback = Box::new(FallbackAdapter::default());
        let adapters: Vec<Box<dyn InferenceAdapter>> = vec![llama, fallback];
        let router = Arc::new(InferenceRouter::new(cfg, adapters));
        router.bootstrap();
        router
    }

    const META_BUNDLE: &str = r#"{"recap":"The session highlights the vendor decision and a few tasks.","decisions":[],"open_questions":[],"active_tasks":[]}"#;
    const CLEAN_BUNDLE: &str = r#"{"recap":"Chose vendor X and committed to signing the contract by Friday.","decisions":["chose vendor X"],"open_questions":[],"active_tasks":["sign by Friday"]}"#;
    // Clean, grounded recap (so no retry) but the model copied the prompt's
    // one-shot exemplar placeholder into the decisions list.
    const LEAK_BUNDLE: &str = r#"{"recap":"Chose vendor X and committed to signing the contract by Friday.","decisions":["chose vendor X","EXAMPLE_DECISION"],"open_questions":[],"active_tasks":["sign by Friday"]}"#;

    fn vendor_inputs() -> SynthesisInputs {
        SynthesisInputs {
            observations: vec![
                observation(ObservationRowKind::Decision, "chose vendor X"),
                observation(ObservationRowKind::Task, "sign by Friday"),
            ],
            recap_seed: "vendor selection".into(),
        }
    }

    #[test]
    fn low_quality_first_attempt_retries_and_keeps_better() {
        // First attempt is meta-commentary (low quality); retry returns a
        // clean factual recap that out-scores it. The synthesizer must
        // keep the clean bundle and record one retry + one low-quality.
        let budgets = Arc::new(Mutex::new(Vec::new()));
        let router = build_router_with_client(SequencedClient::new(
            &[META_BUNDLE, CLEAN_BUNDLE],
            Arc::clone(&budgets),
        ));
        let synth = LlamaCppSynthesizer::new(router);

        let object = synth
            .synthesize(&fresh_window(), &vendor_inputs())
            .expect("synth ok");
        let bundle: SummaryBundle = serde_json::from_slice(&object.payload).unwrap();
        assert_eq!(
            bundle.recap,
            "Chose vendor X and committed to signing the contract by Friday."
        );

        let snap = synth.metrics_snapshot();
        assert_eq!(snap.retry_total, 1);
        assert_eq!(snap.lowquality_total, 1);
        assert_eq!(snap.truncated_total, 0);
        assert_eq!(snap.recap_length_count, 1);

        // Two attempts; the retry budget strictly exceeds the first.
        let budgets = budgets.lock().expect("budgets").clone();
        assert_eq!(budgets.len(), 2, "expected exactly one retry");
        assert!(
            budgets[1] > budgets[0],
            "retry budget {} must exceed first {}",
            budgets[1],
            budgets[0]
        );
    }

    #[test]
    fn high_quality_first_attempt_does_not_retry() {
        let budgets = Arc::new(Mutex::new(Vec::new()));
        let router =
            build_router_with_client(SequencedClient::new(&[CLEAN_BUNDLE], Arc::clone(&budgets)));
        let synth = LlamaCppSynthesizer::new(router);

        synth
            .synthesize(&fresh_window(), &vendor_inputs())
            .expect("synth ok");

        let snap = synth.metrics_snapshot();
        assert_eq!(snap.retry_total, 0);
        assert_eq!(snap.lowquality_total, 0);
        assert_eq!(
            budgets.lock().expect("budgets").len(),
            1,
            "no retry expected"
        );
    }

    #[test]
    fn adaptive_budget_threads_rowcount_to_client() {
        // A clean response means no retry, so exactly one budget is
        // recorded and it must equal the row-count-scaled budget.
        let budgets = Arc::new(Mutex::new(Vec::new()));
        let router =
            build_router_with_client(SequencedClient::new(&[CLEAN_BUNDLE], Arc::clone(&budgets)));
        let synth = LlamaCppSynthesizer::new(router);

        let inputs = vendor_inputs(); // 2 observation rows
        synth
            .synthesize(&fresh_window(), &inputs)
            .expect("synth ok");

        let budgets = budgets.lock().expect("budgets").clone();
        assert_eq!(budgets.len(), 1);
        assert_eq!(budgets[0], crate::quality::adaptive_budget(2));
    }

    #[test]
    fn truncated_output_increments_truncated_metric() {
        // Strict JSON parse fails (no closing quote/brace) but the
        // salvage parser recovers a usable recap -> truncated metric.
        let truncated =
            r#"{"recap":"Adopted Postgres for the billing store and scheduled the migration"#;
        let budgets = Arc::new(Mutex::new(Vec::new()));
        let router =
            build_router_with_client(SequencedClient::new(&[truncated], Arc::clone(&budgets)));
        let synth = LlamaCppSynthesizer::new(router);

        let object = synth
            .synthesize(&fresh_window(), &SynthesisInputs::default())
            .expect("synth ok");
        let bundle: SummaryBundle = serde_json::from_slice(&object.payload).unwrap();
        assert!(bundle.recap.starts_with("Adopted Postgres"));

        let snap = synth.metrics_snapshot();
        assert_eq!(snap.truncated_total, 1);
    }

    #[test]
    fn leaked_exemplar_entry_is_stripped_and_counted() {
        // A clean, grounded recap (so the gate does not retry) whose
        // decisions list carries a leaked exemplar placeholder. The
        // persisted bundle must have the placeholder removed and the
        // server-side counter must record the scrubbed entry.
        let budgets = Arc::new(Mutex::new(Vec::new()));
        let router =
            build_router_with_client(SequencedClient::new(&[LEAK_BUNDLE], Arc::clone(&budgets)));
        let synth = LlamaCppSynthesizer::new(router);

        let object = synth
            .synthesize(&fresh_window(), &vendor_inputs())
            .expect("synth ok");
        let bundle: SummaryBundle = serde_json::from_slice(&object.payload).unwrap();
        assert_eq!(
            bundle.decisions,
            vec!["chose vendor X".to_string()],
            "leaked exemplar placeholder must be scrubbed before persistence"
        );

        let snap = synth.metrics_snapshot();
        assert_eq!(snap.exemplar_leaks_stripped_total, 1);
        // The recap was clean and grounded, so no retry was needed.
        assert_eq!(snap.retry_total, 0);
        assert_eq!(budgets.lock().expect("budgets").len(), 1, "no retry");
    }

    #[test]
    fn shared_metrics_aggregate_across_synthesizers() {
        let metrics = SynthesisMetrics::new();
        let budgets = Arc::new(Mutex::new(Vec::new()));
        let router_a = build_router_with_client(SequencedClient::new(
            &[META_BUNDLE, CLEAN_BUNDLE],
            Arc::clone(&budgets),
        ));
        let router_b = build_router_with_client(SequencedClient::new(
            &[META_BUNDLE, CLEAN_BUNDLE],
            Arc::clone(&budgets),
        ));
        let synth_a = LlamaCppSynthesizer::new(router_a).with_metrics(Arc::clone(&metrics));
        let synth_b = LlamaCppSynthesizer::new(router_b).with_metrics(Arc::clone(&metrics));

        synth_a
            .synthesize(&fresh_window(), &vendor_inputs())
            .unwrap();
        synth_b
            .synthesize(&fresh_window(), &vendor_inputs())
            .unwrap();

        let snap = metrics.snapshot();
        assert_eq!(snap.retry_total, 2, "both synthesizers fold into one total");
        assert_eq!(snap.lowquality_total, 2);
    }
}
