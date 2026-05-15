//! Inference task taxonomy + prompt / grammar templates.

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
                // Aligns with `synthesis_pipeline::SummaryBundle` —
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

/// GBNF for [`synthesis_pipeline::SummaryBundle`] — constrains the
/// SLM to emit JSON with exactly the four fields
/// `{recap, decisions, open_questions, active_tasks}` in order.
///
/// Hand-written from the `SummaryBundle` struct definition; if the
/// struct grows a new field or reorders existing fields the
/// grammar must be updated in lock-step.
pub const GRAMMAR_SYNTH_SUMMARY: &str = r#"
root ::= "{" ws "\"recap\":" ws string "," ws "\"decisions\":" ws strings "," ws "\"open_questions\":" ws strings "," ws "\"active_tasks\":" ws strings ws "}"
strings ::= "[" ws (string ("," ws string)*)? ws "]"
string ::= "\"" ([^"\\] | "\\" .)* "\""
ws ::= [ \t\n]*
"#;

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
