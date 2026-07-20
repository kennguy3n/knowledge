//! [`HybridSynthesizer`] — two-stage synthesis using NER extraction +
//! SLM rephrasing.
//!
//! This module implements the "Hybrid Extraction + SLM Polish"
//! architecture:
//!
//! 1. **Stage 1 — Extraction**: Run [`ner_engine::NerExtractor`] over
//!    the input text to extract entities, decisions, tasks, questions,
//!    and facts. This is deterministic and multilingual — no SLM is
//!    involved.
//! 2. **Stage 2 — Rephrase**: Build a rephrase prompt from the
//!    extracted facts and dispatch it to the SLM via
//!    [`InferenceTask::SynthSummaryRephrase`]. The SLM connects the
//!    facts into fluent prose, constrained by a GBNF grammar to emit
//!    a [`SummaryBundle`].
//!
//! **Fallback path** (Low tier or SLM unavailable): When the SLM is
//! not available, the synthesizer falls back to template-filling —
//! it constructs a [`SummaryBundle`] directly from the extracted facts
//! without any SLM call. This ensures the hybrid path works on all
//! device tiers.
//!
//! # Benefits over `LlamaCppSynthesizer`
//!
//! - **Reduced generation**: The SLM rephrases ~100 tokens of
//!   extracted facts instead of generating ~800 tokens from scratch.
//! - **Coverage by construction**: Entities are extracted in Stage 1
//!   and passed to Stage 2, so they cannot be "forgotten."
//! - **In-language preserved**: Extraction and rephrasing occur in
//!   the original language.
//! - **Non-Latin script support**: XLM-RoBERTa's multilingual
//!   capabilities handle non-Latin scripts that the SLM may struggle
//!   with.

use std::sync::Arc;

use evidence_store::ScopeId;
use inference_router::{
    InferenceRouter, InferenceTask, ModelClass, SamplingConfig, SummaryBundle,
};
use uuid::Uuid;

use crate::error::{PipelineError, Result};
use crate::metrics::SynthesisMetrics;
use crate::object::{SynthesisObject, SynthesisObjectType};
use crate::pipeline::{SynthesisInputs, SynthesisPipeline};
use crate::quality::{salient_terms_from_texts, verify_and_retry, Attempt};
use crate::window::SynthesisWindow;

/// Two-stage synthesizer: NER extraction + SLM rephrasing.
///
/// Implements the [`SynthesisPipeline`] trait. When the SLM is
/// available, Stage 2 dispatches a `SynthSummaryRephrase` task. When
/// the SLM is unavailable (Low tier or no adapter), the synthesizer
/// falls back to template-filling from the extracted facts.
pub struct HybridSynthesizer {
    /// NER extractor for Stage 1.
    ner: Arc<ner_engine::NerExtractor>,
    /// Inference router for Stage 2 SLM dispatch.
    router: Arc<InferenceRouter>,
    /// Object type to emit.
    object_type: SynthesisObjectType,
    /// Provenance reference stamped on emitted objects.
    provenance_ref: Uuid,
    /// Shared synthesis quality metrics.
    metrics: Arc<SynthesisMetrics>,
}

impl HybridSynthesizer {
    /// Build a hybrid synthesizer wrapping the given NER extractor and
    /// inference router. The caller is responsible for calling
    /// [`InferenceRouter::bootstrap`] before the first synthesis run.
    pub fn new(ner: Arc<ner_engine::NerExtractor>, router: Arc<InferenceRouter>) -> Self {
        Self {
            ner,
            router,
            object_type: SynthesisObjectType::ChannelRecap,
            provenance_ref: Uuid::nil(),
            metrics: SynthesisMetrics::new(),
        }
    }

    /// Share an externally-owned [`SynthesisMetrics`] so several
    /// synthesizers fold their quality counters into the same totals.
    #[must_use]
    pub fn with_metrics(mut self, metrics: Arc<SynthesisMetrics>) -> Self {
        self.metrics = metrics;
        self
    }

    /// Borrow the shared metrics handle.
    #[must_use]
    pub fn metrics(&self) -> &Arc<SynthesisMetrics> {
        &self.metrics
    }

    /// Point-in-time snapshot of the synthesis-quality counters.
    #[must_use]
    pub fn metrics_snapshot(&self) -> crate::metrics::SynthesisMetricsSnapshot {
        self.metrics.snapshot()
    }

    /// Override the [`SynthesisObjectType`] emitted by
    /// [`Self::synthesize`].
    pub fn with_object_type(mut self, object_type: SynthesisObjectType) -> Self {
        self.object_type = object_type;
        self
    }

    /// Override the provenance ref stamped on every emitted object.
    pub fn with_provenance_ref(mut self, provenance_ref: Uuid) -> Self {
        self.provenance_ref = provenance_ref;
        self
    }

    /// Borrow the underlying router (mostly useful for tests).
    pub fn router(&self) -> &InferenceRouter {
        &self.router
    }

    /// Stage 1: Extract facts from the input text using the NER
    /// extractor.
    ///
    /// Runs all three extraction passes (XLM-R NER, lexicon, regex)
    /// and returns the merged, deduplicated facts.
    fn extract_facts(&self, inputs: &SynthesisInputs) -> ner_engine::ExtractedFacts {
        let scope = ScopeId::new_v4();
        let combined = inputs
            .observations
            .iter()
            .map(|row| row.content.as_str())
            .chain(std::iter::once(inputs.recap_seed.as_str()))
            .collect::<Vec<_>>()
            .join("\n\n");

        self.ner.extract_all(&combined, scope)
    }

    /// Format extracted facts into a text body for the
    /// [`InferenceTask::SynthSummaryRephrase`] prompt template's
    /// `{body}` placeholder. Delegates to
    /// [`ner_engine::ExtractedFacts::format_rephrase_body`].
    pub fn format_rephrase_body(facts: &ner_engine::ExtractedFacts) -> String {
        facts.format_rephrase_body()
    }

    /// Stage 2 (fallback): Template-fill a [`SummaryBundle`] directly
    /// from the extracted facts, without any SLM call.
    ///
    /// This is used when the SLM is unavailable (Low tier, no adapter)
    /// or when the SLM dispatch fails. The resulting bundle is
    /// deterministic — the recap is a simple join of the facts, and
    /// the structured lists are copied verbatim.
    fn template_fill(facts: &ner_engine::ExtractedFacts) -> SummaryBundle {
        let recap = if !facts.facts.is_empty() {
            facts.facts.join(". ")
        } else if !facts.entities.is_empty() {
            let entity_names: Vec<&str> =
                facts.entities.iter().map(|e| e.content.as_str()).collect();
            format!("Key entities: {}.", entity_names.join(", "))
        } else {
            "No facts extracted.".to_string()
        };

        SummaryBundle {
            recap,
            decisions: facts.decisions.clone(),
            open_questions: facts.questions.clone(),
            active_tasks: facts.tasks.clone(),
        }
    }

    /// Run one SLM rephrase attempt: dispatch the rephrase prompt with
    /// the given sampling config, parse the bundle, and report
    /// truncation.
    fn run_rephrase_attempt(
        &self,
        prompt: &str,
        sampling: &SamplingConfig,
    ) -> Result<Attempt> {
        let raw = self
            .router
            .dispatch_with_sampling(InferenceTask::SynthSummaryRephrase, prompt, sampling)
            .map_err(|e| PipelineError::SynthesisFailed(e.to_string()))?;

        let (bundle, truncated) = SummaryBundle::from_slm_str_salvaged(&raw).map_err(|e| {
            PipelineError::SynthesisFailed(format!(
                "SLM output did not parse as SummaryBundle: {e}; raw=`{raw}`"
            ))
        })?;
        Ok(Attempt { bundle, truncated })
    }
}

impl SynthesisPipeline for HybridSynthesizer {
    fn synthesize(
        &self,
        window: &SynthesisWindow,
        inputs: &SynthesisInputs,
    ) -> Result<SynthesisObject> {
        // ── Stage 1: Extraction ────────────────────────────────────
        let facts = self.extract_facts(inputs);

        if facts.is_empty() {
            // No facts extracted — emit an empty bundle.
            let bundle = SummaryBundle::default();
            let payload = serde_json::to_vec(&bundle)
                .map_err(|_| PipelineError::Serialisation("SummaryBundle::to_vec"))?;
            return Ok(SynthesisObject::new(
                window.scope_id,
                window.id,
                self.object_type,
                payload,
                self.provenance_ref,
            ));
        }

        // ── Stage 2: Rephrase (or template-fill fallback) ───────────
        // Check if the SLM is available. If not, template-fill.
        let rephrase_body = Self::format_rephrase_body(&facts);
        let model_class = ModelClass::from_model_path(&self.router.config().model_path);
        let prompt = InferenceTask::SynthSummaryRephrase
            .prompt_template_for_class(model_class)
            .replace("{body}", &rephrase_body);

        // Salient terms from the extracted facts for quality scoring.
        let salient = salient_terms_from_texts(
            facts
                .facts
                .iter()
                .map(String::as_str)
                .chain(facts.decisions.iter().map(String::as_str))
                .chain(facts.tasks.iter().map(String::as_str))
                .chain(facts.questions.iter().map(String::as_str))
                .chain(
                    facts
                        .entities
                        .iter()
                        .map(|e| e.content.as_str()),
                ),
        );

        let base = self.router.config().sampling;

        // Try SLM rephrase with verify-and-retry. If the dispatch
        // fails (no adapter, tier too low), fall back to template-fill.
        let verified = match verify_and_retry(
            &prompt,
            inputs.observations.len(),
            &salient,
            |attempt_prompt, n_predict| {
                self.run_rephrase_attempt(attempt_prompt, &base.with_n_predict(n_predict))
            },
        ) {
            Ok(v) => v,
            Err(_) => {
                // SLM unavailable — template-fill from extracted facts.
                let bundle = Self::template_fill(&facts);
                let payload = serde_json::to_vec(&bundle)
                    .map_err(|_| PipelineError::Serialisation("SummaryBundle::to_vec"))?;
                return Ok(SynthesisObject::new(
                    window.scope_id,
                    window.id,
                    self.object_type,
                    payload,
                    self.provenance_ref,
                ));
            }
        };

        // Update metrics.
        if verified.low_quality {
            self.metrics.incr_lowquality();
        }
        if verified.retried {
            self.metrics.incr_retry();
        }
        if verified.retry_failed {
            self.metrics.incr_retry_failed();
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{ImportanceTagClass, ObservationRow, ObservationRowKind};
    use chrono::{Duration, Utc};
    use evidence_store::ScopeId;
    use inference_router::adapter::InferenceAdapter;
    use inference_router::adapters::llama_cpp::MockLlamaServerClient;
    use inference_router::{
        DeviceTier, FallbackAdapter, InferenceRouter, LlamaCppAdapter, RouterConfig,
    };

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
        let cfg = RouterConfig::default().with_device_tier(DeviceTier::High);
        let llama = Box::new(LlamaCppAdapter::new(cfg.clone(), Box::new(client)));
        let fallback = Box::new(FallbackAdapter::default());
        let adapters: Vec<Box<dyn InferenceAdapter>> = vec![llama, fallback];
        let router = Arc::new(InferenceRouter::new(cfg, adapters));
        router.bootstrap();
        router
    }

    fn build_ner_extractor() -> Arc<ner_engine::NerExtractor> {
        Arc::new(ner_engine::NerExtractor::new())
    }

    #[test]
    fn hybrid_synthesizer_round_trips_summary_bundle() {
        let response = r#"{"recap":"Picked vendor X.","decisions":["chose vendor X"],"open_questions":[],"active_tasks":["sign by Friday"]}"#;
        let router = build_router_with_llama(MockLlamaServerClient::ok(response));
        let ner = build_ner_extractor();
        let synth = HybridSynthesizer::new(ner, router);
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
    fn hybrid_synthesizer_falls_back_to_template_fill_on_unavailable() {
        // No reachable adapter -> dispatch returns Unavailable ->
        // synthesizer falls back to template-fill.
        let router = build_router_with_llama(MockLlamaServerClient::unreachable());
        let ner = build_ner_extractor();
        let synth = HybridSynthesizer::new(ner, router);
        let window = fresh_window();
        let inputs = SynthesisInputs {
            observations: vec![
                observation(ObservationRowKind::Decision, "approved the budget"),
                observation(ObservationRowKind::Task, "send invoice"),
            ],
            recap_seed: "budget discussion".into(),
        };
        let object = synth.synthesize(&window, &inputs).expect("template-fill ok");
        let bundle: SummaryBundle = serde_json::from_slice(&object.payload).unwrap();
        // Template-fill should contain the extracted decisions and tasks.
        assert!(
            bundle.decisions.iter().any(|d| d.contains("approved") || d.contains("budget")),
            "decisions should contain extracted decision: {:?}",
            bundle.decisions
        );
        assert!(
            bundle.active_tasks.iter().any(|t| t.contains("invoice") || t.contains("send")),
            "tasks should contain extracted task: {:?}",
            bundle.active_tasks
        );
    }

    #[test]
    fn hybrid_synthesizer_respects_object_type_override() {
        let response = r#"{"recap":"r","decisions":[],"open_questions":[],"active_tasks":[]}"#;
        let router = build_router_with_llama(MockLlamaServerClient::ok(response));
        let ner = build_ner_extractor();
        let synth = HybridSynthesizer::new(ner, router)
            .with_object_type(SynthesisObjectType::DomainSummary);
        let object = synth
            .synthesize(&fresh_window(), &SynthesisInputs::from_recap("test"))
            .unwrap();
        assert_eq!(object.object_type, SynthesisObjectType::DomainSummary);
    }

    #[test]
    fn hybrid_synthesizer_emits_empty_bundle_for_empty_inputs() {
        let router = build_router_with_llama(MockLlamaServerClient::ok(
            r#"{"recap":"","decisions":[],"open_questions":[],"active_tasks":[]}"#,
        ));
        let ner = build_ner_extractor();
        let synth = HybridSynthesizer::new(ner, router);
        let object = synth
            .synthesize(&fresh_window(), &SynthesisInputs::default())
            .unwrap();
        let bundle: SummaryBundle = serde_json::from_slice(&object.payload).unwrap();
        assert!(bundle.recap.is_empty() || bundle.recap == "No facts extracted.");
    }

    #[test]
    fn template_fill_constructs_bundle_from_facts() {
        let facts = ner_engine::ExtractedFacts {
            entities: vec![ner_engine::ExtractedEntity {
                content: "Acme Corp".into(),
                entity_type: ner_engine::EntityType::Organization,
                confidence: 0.9,
                source: ner_engine::EntitySource::Ner,
            }],
            decisions: vec!["approved budget".into()],
            tasks: vec!["send invoice".into()],
            questions: vec!["when is the deadline?".into()],
            facts: vec!["the budget is $5M".into()],
            dominant_language: Some("en".into()),
        };
        let bundle = HybridSynthesizer::template_fill(&facts);
        assert!(bundle.recap.contains("the budget is $5M"));
        assert_eq!(bundle.decisions, vec!["approved budget".to_string()]);
        assert_eq!(bundle.active_tasks, vec!["send invoice".to_string()]);
        assert_eq!(bundle.open_questions, vec!["when is the deadline?".to_string()]);
    }

    #[test]
    fn format_rephrase_body_includes_all_fact_categories() {
        let facts = ner_engine::ExtractedFacts {
            entities: vec![ner_engine::ExtractedEntity {
                content: "John Smith".into(),
                entity_type: ner_engine::EntityType::Person,
                confidence: 0.9,
                source: ner_engine::EntitySource::Ner,
            }],
            decisions: vec!["approved the proposal".into()],
            tasks: vec!["follow up next week".into()],
            questions: vec!["who owns the migration?".into()],
            facts: vec!["the deadline is Friday".into()],
            dominant_language: Some("en".into()),
        };
        let prompt = HybridSynthesizer::format_rephrase_body(&facts);
        assert!(prompt.contains("John Smith"));
        assert!(prompt.contains("approved the proposal"));
        assert!(prompt.contains("follow up next week"));
        assert!(prompt.contains("who owns the migration?"));
        assert!(prompt.contains("the deadline is Friday"));
    }
}
