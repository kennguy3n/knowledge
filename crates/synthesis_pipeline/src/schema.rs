//! GBNF-shaped structured-output schema types.
//!
//! Per `docs/technical/architecture.md` §3.5: "every structured output is generated
//! with GBNF grammar-constrained decoding ... the substrate never has
//! to repair malformed JSON from the SLM at the consumer side".
//!
//! The deliverable is the **shape** of those outputs as Rust
//! types. The actual GBNF grammar files live alongside the prompt
//! catalog in the inference pipeline; the on-device `llama-server`
//! constrains the SLM to emit exactly these JSON shapes, and the
//! consumer in this crate deserialises them with `serde_json`.
//!
//! The four types here cover the inference tasks listed in
//! `docs/technical/architecture.md` §3.3:
//!
//! * [`ImportanceTag`] — `tag.importance`
//! * [`EntityList`]    — `extract.entities`
//! * [`ObservationRow`] — `promote.observation`
//! * [`SummaryBundle`]  — `synth.summary`

use serde::{Deserialize, Serialize};

/// Importance class as emitted by the SLM (mirrors
/// `evidence_store::ImportanceClass` but is duplicated here so this
/// crate does not pull in the full evidence-store dependency only for
/// schema types).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportanceTagClass {
    /// Tenant policy, regulatory rules, signed decisions.
    Critical,
    /// Owners, project commitments, canonical concepts.
    Important,
    /// Recurring tasks, channel recaps, workflows.
    Useful,
    /// Greetings, social chatter, transient pings.
    Noise,
}

/// Output shape for `tag.importance` — `{class, confidence}` JSON.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImportanceTag {
    /// The importance class.
    pub class: ImportanceTagClass,
    /// Model confidence in `0.0 ..= 1.0`.
    pub confidence: f64,
}

/// Entity types the substrate tracks at extraction time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntityType {
    /// Named person.
    Person,
    /// Named organisation / project / team.
    Organization,
    /// Geographic location.
    Location,
    /// Date or time reference (`"by Friday"`, `"Q3 2026"`).
    DateTime,
    /// Numeric quantity (`"$5M"`, `"3 sprints"`).
    Quantity,
    /// URL.
    Url,
    /// Email address.
    Email,
    /// `@`-style mention.
    Mention,
    /// `#`-style hashtag.
    Hashtag,
    /// Other / catch-all.
    Other,
}

/// One entry in [`EntityList`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EntityRecord {
    /// Entity type.
    pub kind: EntityType,
    /// Surface span the SLM extracted.
    pub span: String,
    /// Model confidence in `0.0 ..= 1.0`.
    pub confidence: f64,
}

/// Output shape for `extract.entities` — `[{type, span, confidence}]`
/// JSON.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EntityList {
    /// Extracted entities.
    pub entities: Vec<EntityRecord>,
}

impl EntityList {
    /// Convenience: build from a vec.
    pub fn from_records(entities: Vec<EntityRecord>) -> Self {
        Self { entities }
    }
}

/// Observation row kind — mirrors
/// `observation_engine::ObservationType` so the SLM can emit the same
/// taxonomy as the lexicon extractor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationRowKind {
    /// Named entity.
    Entity,
    /// Declarative fact.
    Fact,
    /// Action item / task.
    Task,
    /// Explicit decision.
    Decision,
    /// Claim that hasn't been corroborated yet.
    Claim,
    /// Open question.
    Question,
}

/// Output shape for `promote.observation` — one structured row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObservationRow {
    /// What kind of observation this is.
    pub kind: ObservationRowKind,
    /// Canonical surface form.
    pub content: String,
    /// Importance tag (re-classification by the promote pass).
    pub importance: ImportanceTagClass,
    /// Model confidence in `0.0 ..= 1.0`.
    pub confidence: f64,
}

/// Output shape for `synth.summary` — channel / episodic / domain /
/// tenant summary bundle.
///
/// The canonical definition lives in
/// [`inference_router::task::SummaryBundle`] so that both the
/// synthesiser (`LlamaCppSynthesizer` in this crate) and the
/// episodic-memory consumer
/// (`memory_manager::episodic::SlmSummarizer`) share a single type
/// without `memory_manager` having to depend on this crate. Re-exported
/// here so existing consumers that imported it from
/// `synthesis_pipeline::schema` keep compiling.
pub use inference_router::SummaryBundle;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn importance_tag_round_trips_through_serde_json() {
        let t = ImportanceTag {
            class: ImportanceTagClass::Important,
            confidence: 0.92,
        };
        let s = serde_json::to_string(&t).unwrap();
        let back: ImportanceTag = serde_json::from_str(&s).unwrap();
        assert_eq!(back, t);
    }

    #[test]
    fn entity_list_round_trips_through_serde_json() {
        let list = EntityList::from_records(vec![EntityRecord {
            kind: EntityType::Person,
            span: "Sara".into(),
            confidence: 0.81,
        }]);
        let s = serde_json::to_string(&list).unwrap();
        let back: EntityList = serde_json::from_str(&s).unwrap();
        assert_eq!(back, list);
    }
}
