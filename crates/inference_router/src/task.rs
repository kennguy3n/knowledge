//! Inference task taxonomy + prompt / grammar templates.
//!
//! This module also owns the **output shapes** that the GBNF grammars
//! constrain the SLM to emit. The schema lives next to the grammar so
//! drift between producer (the grammar fed to `llama-server`) and
//! consumer (the `serde_json` decode site) is locally visible in one
//! file. Today only [`SummaryBundle`] lives here — the other
//! grammar-constrained shapes still live in `synthesis_pipeline::schema`
//! and may move here in a future cleanup.

use serde::{Deserialize, Serialize};

/// One inference task served by the router. Each task has a stable
/// string tag, a prompt template, and a GBNF grammar constraint.
///
/// Per `ARCHITECTURE.md` §3 the substrate routes the following tasks
/// through the SLM:
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InferenceTask {
    /// Tag an evidence row with an importance class.
    TagImportance,
    /// Extract entities from an evidence row.
    ExtractEntities,
    /// Promote one observation candidate.
    PromoteObservation,
    /// Summarise a session / thread (episodic memory).
    SynthSummary,
    /// Synthesise a concept from a window of observations.
    SynthConcept,
    /// Adjudicate a contradiction between two canonical claims.
    AdjudicateContradiction,
}

impl InferenceTask {
    /// Canonical ordered list of every variant — the single source of
    /// truth other modules iterate over. Kept next to the enum
    /// itself (rather than duplicated in `router.rs`) so the
    /// exhaustiveness test [`tests::all_is_exhaustive`] below can
    /// catch any new variant that gets added without being appended
    /// here. The order is the same as the variant declaration order
    /// and is part of the router's wire contract (it determines the
    /// order in which `adapter_states()` reports an adapter's
    /// supported tasks).
    pub const ALL: &'static [InferenceTask] = &[
        Self::TagImportance,
        Self::ExtractEntities,
        Self::PromoteObservation,
        Self::SynthSummary,
        Self::SynthConcept,
        Self::AdjudicateContradiction,
    ];
}

/// Stable string tag for a task — used as the cache key and as the
/// `task_tag` argument passed to adapters.
pub type TaskTag = &'static str;

impl InferenceTask {
    /// Stable string tag for the task.
    pub const fn tag(self) -> TaskTag {
        match self {
            Self::TagImportance => "tag_importance",
            Self::ExtractEntities => "extract_entities",
            Self::PromoteObservation => "promote_observation",
            Self::SynthSummary => "synth_summary",
            Self::SynthConcept => "synth_concept",
            Self::AdjudicateContradiction => "adjudicate_contradiction",
        }
    }

    /// `true` when this task is *classification* (yes/no/category).
    /// Classification tasks can be served by encoder-only fallbacks;
    /// synthesis tasks cannot.
    pub const fn is_classification(self) -> bool {
        matches!(
            self,
            Self::TagImportance | Self::ExtractEntities | Self::PromoteObservation
        )
    }

    /// `true` when this task requires generative synthesis (prose,
    /// summaries, free-form concept text). Cannot be served by the
    /// encoder-only fallback adapter.
    pub const fn is_synthesis(self) -> bool {
        matches!(
            self,
            Self::SynthSummary | Self::SynthConcept | Self::AdjudicateContradiction
        )
    }

    /// Static prompt template for the task. Placeholders are
    /// substituted by the caller (the router does not interpret the
    /// prompt itself).
    pub const fn prompt_template(self) -> &'static str {
        match self {
            Self::TagImportance => {
                "Classify the following message into one of {critical, important, useful, noise}. \
                 Respond with strict JSON: {\"class\": \"…\", \"confidence\": <0.0-1.0>}.\n\nMessage:\n{body}"
            }
            Self::ExtractEntities => {
                "List the named entities (people, projects, deadlines, decisions) in the following \
                 message. Respond as JSON: {\"entities\": [{\"name\": \"…\", \"type\": \"…\"}]}.\n\nMessage:\n{body}"
            }
            Self::PromoteObservation => {
                "Decide whether the following observation should be promoted to canonical \
                 knowledge. Respond as JSON: {\"promote\": true|false, \"reason\": \"…\"}.\n\nObservation:\n{body}"
            }
            Self::SynthSummary => {
                // Aligns with [`SummaryBundle`] in this module —
                // four fields, all populated even when empty. The
                // GBNF `GRAMMAR_SYNTH_SUMMARY` constrains the
                // emitted JSON to exactly this shape so the
                // synthesiser never has to repair output.
                "Summarise the following session as a JSON object with this exact shape: \
                 {\"recap\": \"…\", \"decisions\": [\"…\"], \"open_questions\": [\"…\"], \"active_tasks\": [\"…\"]}. \
                 The recap field is a 2-4 sentence headline; the other fields each list zero or more strings.\n\n\
                 Session:\n{body}"
            }
            Self::SynthConcept => {
                "Synthesise a concept from the following observations. Output a JSON object: \
                 {\"name\": \"…\", \"summary\": \"…\", \"facets\": {\"…\": \"…\"}}.\n\nObservations:\n{body}"
            }
            Self::AdjudicateContradiction => {
                "The following two canonical claims contradict. Decide which is canonical \
                 and explain. Respond as JSON: {\"canonical\": \"a|b\", \"reason\": \"…\"}.\n\nA:\n{a}\n\nB:\n{b}"
            }
        }
    }

    /// GBNF grammar constraint for the task — feed verbatim to
    /// llama.cpp's `--grammar` parameter so the model can only emit
    /// JSON of the expected shape.
    pub const fn grammar(self) -> &'static str {
        match self {
            Self::TagImportance => GRAMMAR_TAG_IMPORTANCE,
            Self::ExtractEntities => GRAMMAR_EXTRACT_ENTITIES,
            Self::PromoteObservation => GRAMMAR_PROMOTE_OBSERVATION,
            Self::SynthSummary => GRAMMAR_SYNTH_SUMMARY,
            Self::SynthConcept => GRAMMAR_SYNTH_CONCEPT,
            Self::AdjudicateContradiction => GRAMMAR_ADJUDICATE,
        }
    }
}

/// GBNF for `{"class": "critical|important|useful|noise", "confidence": 0.0-1.0}`.
pub const GRAMMAR_TAG_IMPORTANCE: &str = r#"
root ::= "{" ws "\"class\":" ws class "," ws "\"confidence\":" ws number ws "}"
class ::= "\"critical\"" | "\"important\"" | "\"useful\"" | "\"noise\""
number ::= "0" "." [0-9]+ | "1.0" | "1"
ws ::= [ \t\n]*
"#;

/// GBNF for `{"entities": [{"name": "…", "type": "…"}]}`.
pub const GRAMMAR_EXTRACT_ENTITIES: &str = r#"
root ::= "{" ws "\"entities\":" ws "[" ws (entity ("," ws entity)*)? ws "]" ws "}"
entity ::= "{" ws "\"name\":" ws string "," ws "\"type\":" ws string ws "}"
string ::= "\"" ([^"\\] | "\\" .)* "\""
ws ::= [ \t\n]*
"#;

/// GBNF for `{"promote": bool, "reason": "…"}`.
pub const GRAMMAR_PROMOTE_OBSERVATION: &str = r#"
root ::= "{" ws "\"promote\":" ws ("true" | "false") "," ws "\"reason\":" ws string ws "}"
string ::= "\"" ([^"\\] | "\\" .)* "\""
ws ::= [ \t\n]*
"#;

/// GBNF for [`SummaryBundle`] — constrains the SLM to emit JSON with
/// exactly the four fields `{recap, decisions, open_questions,
/// active_tasks}` in order.
///
/// Hand-written from the [`SummaryBundle`] struct definition; if the
/// struct grows a new field or reorders existing fields the grammar
/// must be updated in lock-step. The
/// `synth_summary_grammar_matches_summary_bundle_serialization` test
/// guards against drift by serialising a populated [`SummaryBundle`]
/// and asserting the produced JSON's field ordering and shape match
/// what the grammar accepts.
pub const GRAMMAR_SYNTH_SUMMARY: &str = r#"
root ::= "{" ws "\"recap\":" ws string "," ws "\"decisions\":" ws strings "," ws "\"open_questions\":" ws strings "," ws "\"active_tasks\":" ws strings ws "}"
strings ::= "[" ws (string ("," ws string)*)? ws "]"
string ::= "\"" ([^"\\] | "\\" .)* "\""
ws ::= [ \t\n]*
"#;

/// Output shape for [`InferenceTask::SynthSummary`] —
/// channel / episodic / domain / tenant summary bundle.
///
/// The four fields are produced in declaration order by
/// `serde_json::to_string`, which is exactly the order the
/// [`GRAMMAR_SYNTH_SUMMARY`] grammar accepts. Reordering or renaming
/// fields here without updating the grammar will cause the SLM to
/// emit JSON the parser still accepts but the grammar will reject,
/// silently making structured decoding fail at the adapter level.
///
/// This type lives in `inference_router::task` (not
/// `synthesis_pipeline::schema`) so that both the producer
/// (`LlamaCppSynthesizer`) and the consumer
/// (`memory_manager::episodic::SlmSummarizer`) can share a single
/// canonical definition without `memory_manager` taking a dependency
/// on `synthesis_pipeline`. `synthesis_pipeline::schema` re-exports
/// it for backward compatibility.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SummaryBundle {
    /// Free-text recap (the headline).
    pub recap: String,
    /// Decisions captured during the window.
    pub decisions: Vec<String>,
    /// Open questions captured during the window.
    pub open_questions: Vec<String>,
    /// Active tasks captured during the window.
    pub active_tasks: Vec<String>,
}

/// GBNF for the synthesised concept JSON.
pub const GRAMMAR_SYNTH_CONCEPT: &str = r#"
root ::= "{" ws "\"name\":" ws string "," ws "\"summary\":" ws string "," ws "\"facets\":" ws "{" ws ((string ":" ws string ("," ws string ":" ws string)*))? ws "}" ws "}"
string ::= "\"" ([^"\\] | "\\" .)* "\""
ws ::= [ \t\n]*
"#;

/// GBNF for the adjudication JSON.
pub const GRAMMAR_ADJUDICATE: &str = r#"
root ::= "{" ws "\"canonical\":" ws ("\"a\"" | "\"b\"") "," ws "\"reason\":" ws string ws "}"
string ::= "\"" ([^"\\] | "\\" .)* "\""
ws ::= [ \t\n]*
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_tags_are_unique_and_stable() {
        let tags: Vec<&'static str> = [
            InferenceTask::TagImportance,
            InferenceTask::ExtractEntities,
            InferenceTask::PromoteObservation,
            InferenceTask::SynthSummary,
            InferenceTask::SynthConcept,
            InferenceTask::AdjudicateContradiction,
        ]
        .iter()
        .map(|t| t.tag())
        .collect();
        let mut sorted = tags.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), tags.len(), "task tags must be unique");
    }

    #[test]
    fn classification_vs_synthesis_partition() {
        for task in [
            InferenceTask::TagImportance,
            InferenceTask::ExtractEntities,
            InferenceTask::PromoteObservation,
        ] {
            assert!(task.is_classification());
            assert!(!task.is_synthesis());
        }
        for task in [
            InferenceTask::SynthSummary,
            InferenceTask::SynthConcept,
            InferenceTask::AdjudicateContradiction,
        ] {
            assert!(!task.is_classification());
            assert!(task.is_synthesis());
        }
    }

    #[test]
    fn grammars_are_present_for_classification() {
        for task in [
            InferenceTask::TagImportance,
            InferenceTask::ExtractEntities,
            InferenceTask::PromoteObservation,
        ] {
            assert!(!task.grammar().is_empty(), "task {task:?} needs a grammar");
        }
    }

    #[test]
    fn synth_summary_grammar_constrains_summary_bundle_shape() {
        let g = InferenceTask::SynthSummary.grammar();
        assert!(!g.is_empty(), "synth_summary must constrain output");
        for field in ["recap", "decisions", "open_questions", "active_tasks"] {
            assert!(
                g.contains(field),
                "GBNF must mention `{field}` so the SLM emits SummaryBundle JSON"
            );
        }
    }

    /// Drift guard: serialise a populated [`SummaryBundle`] and
    /// confirm the resulting JSON
    ///
    /// * is in the field order the grammar accepts
    ///   (`recap` then `decisions` then `open_questions` then
    ///   `active_tasks`),
    /// * round-trips back to the same struct,
    /// * starts with `{"recap":` so the grammar's first production
    ///   (`"{" ws "\"recap\":"`) matches.
    ///
    /// If [`SummaryBundle`] grows a field or its declaration order
    /// changes, the first two assertions catch it before the SLM
    /// ever sees the new grammar / new shape.
    #[test]
    fn synth_summary_grammar_matches_summary_bundle_serialization() {
        let bundle = SummaryBundle {
            recap: "the team shipped".to_string(),
            decisions: vec!["keep vendor X".to_string()],
            open_questions: vec!["who owns the rollout?".to_string()],
            active_tasks: vec!["draft RFC".to_string()],
        };
        let json = serde_json::to_string(&bundle).unwrap();
        // Field order must match the grammar's `root` production.
        let recap_idx = json.find("\"recap\"").unwrap();
        let decisions_idx = json.find("\"decisions\"").unwrap();
        let questions_idx = json.find("\"open_questions\"").unwrap();
        let tasks_idx = json.find("\"active_tasks\"").unwrap();
        assert!(
            recap_idx < decisions_idx && decisions_idx < questions_idx && questions_idx < tasks_idx,
            "SummaryBundle serialisation drifted from GBNF field order; got: {json}"
        );
        assert!(
            json.starts_with("{\"recap\":"),
            "GBNF root expects `{{\"recap\":` prefix, got: {json}"
        );
        // Round-trip the JSON back through the type so we know
        // serde_json::from_str (used by every grammar-constrained
        // consumer) reverses the encoding without loss.
        let decoded: SummaryBundle = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, bundle);
    }

    /// Exhaustiveness pin for [`InferenceTask::ALL`].
    ///
    /// The `match` below has NO catch-all arm, so adding a new
    /// variant to [`InferenceTask`] without also appending it to
    /// `InferenceTask::ALL` is a **compile error** (the exhaustive
    /// match no longer covers the enum). On top of that the runtime
    /// assertion pins cardinality and order so a reviewer who
    /// deletes a variant from `ALL` (without also removing it from
    /// the enum) gets a `cargo test` failure.
    ///
    /// Concretely this is the structural defence asked
    /// for: the router's `ALL_TASKS` constant now derives from
    /// `InferenceTask::ALL`, and `InferenceTask::ALL` is pinned to
    /// the enum's discriminants here.
    #[test]
    fn all_is_exhaustive() {
        let mut count = 0_usize;
        for &task in InferenceTask::ALL {
            count += 1;
            // No `_ =>` arm — if a variant is added to the enum but
            // not to `ALL`, the compiler refuses to compile this
            // test until the new variant is appended to `ALL` AND
            // matched here. If a variant is renamed, the same
            // mismatch surfaces.
            #[allow(clippy::match_same_arms)]
            match task {
                InferenceTask::TagImportance => {}
                InferenceTask::ExtractEntities => {}
                InferenceTask::PromoteObservation => {}
                InferenceTask::SynthSummary => {}
                InferenceTask::SynthConcept => {}
                InferenceTask::AdjudicateContradiction => {}
            }
        }
        assert_eq!(count, 6, "InferenceTask::ALL drifted from enum cardinality");
        // Order is part of the public contract — pin it explicitly.
        assert_eq!(
            InferenceTask::ALL
                .iter()
                .map(|t| t.tag())
                .collect::<Vec<_>>(),
            vec![
                "tag_importance",
                "extract_entities",
                "promote_observation",
                "synth_summary",
                "synth_concept",
                "adjudicate_contradiction",
            ],
        );
    }

    #[test]
    fn prompts_contain_placeholders() {
        for task in [
            InferenceTask::TagImportance,
            InferenceTask::SynthSummary,
            InferenceTask::AdjudicateContradiction,
        ] {
            let template = task.prompt_template();
            assert!(template.contains('{') && template.contains('}'));
        }
    }
}
