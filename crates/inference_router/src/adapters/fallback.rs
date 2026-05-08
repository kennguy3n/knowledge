//! Encoder-only fallback adapter.
//!
//! [`FallbackAdapter`] satisfies classification tasks (`TagImportance`,
//! `ExtractEntities`, …) by emitting a deterministic JSON payload that
//! callers can parse without an SLM. Synthesis tasks are rejected via
//! [`crate::RouterError::Unavailable`] so the router signals the
//! caller to fall back to a non-SLM strategy (e.g. concatenated
//! observations from [`crate::task::InferenceTask::SynthSummary`]).

use std::sync::atomic::{AtomicBool, Ordering};

use crate::adapter::{AdapterKind, InferenceAdapter, ProbeResult};
use crate::error::RouterError;
use crate::task::InferenceTask;

/// Encoder-only fallback adapter — always available, classification
/// only.
pub struct FallbackAdapter {
    available: AtomicBool,
}

impl FallbackAdapter {
    /// Construct a new fallback adapter.
    pub fn new() -> Self {
        Self {
            available: AtomicBool::new(true),
        }
    }
}

impl Default for FallbackAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl InferenceAdapter for FallbackAdapter {
    fn kind(&self) -> AdapterKind {
        AdapterKind::Fallback
    }

    fn probe(&self) -> ProbeResult {
        // Always available — encoder-only fallback runs everywhere.
        self.available.store(true, Ordering::SeqCst);
        ProbeResult::Available
    }

    fn is_available(&self) -> bool {
        self.available.load(Ordering::SeqCst)
    }

    fn supports(&self, task: InferenceTask) -> bool {
        task.is_classification()
    }

    fn generate(
        &self,
        task_tag: &str,
        _prompt: &str,
        _grammar: &str,
    ) -> Result<String, RouterError> {
        match task_tag {
            "tag_importance" => Ok(r#"{"class":"useful","confidence":0.5}"#.into()),
            "extract_entities" => Ok(r#"{"entities":[]}"#.into()),
            "promote_observation" => {
                Ok(r#"{"promote":false,"reason":"fallback adapter cannot promote"}"#.into())
            }
            "synth_summary" | "synth_concept" | "adjudicate_contradiction" => {
                Err(RouterError::Unavailable {
                    task: stable_tag(task_tag),
                })
            }
            _ => Err(RouterError::Unavailable { task: "unknown" }),
        }
    }
}

fn stable_tag(task_tag: &str) -> &'static str {
    match task_tag {
        "synth_summary" => "synth_summary",
        "synth_concept" => "synth_concept",
        "adjudicate_contradiction" => "adjudicate_contradiction",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_is_always_available() {
        let adapter = FallbackAdapter::new();
        assert_eq!(adapter.probe(), ProbeResult::Available);
        assert!(adapter.is_available());
    }

    #[test]
    fn fallback_supports_only_classification() {
        let adapter = FallbackAdapter::new();
        assert!(adapter.supports(InferenceTask::TagImportance));
        assert!(adapter.supports(InferenceTask::ExtractEntities));
        assert!(adapter.supports(InferenceTask::PromoteObservation));
        assert!(!adapter.supports(InferenceTask::SynthSummary));
        assert!(!adapter.supports(InferenceTask::SynthConcept));
        assert!(!adapter.supports(InferenceTask::AdjudicateContradiction));
    }

    #[test]
    fn fallback_classification_returns_useful_with_low_confidence() {
        let adapter = FallbackAdapter::new();
        let out = adapter.generate("tag_importance", "", "").unwrap();
        assert!(out.contains("useful"));
        assert!(out.contains("0.5"));
    }

    #[test]
    fn fallback_synthesis_returns_unavailable() {
        let adapter = FallbackAdapter::new();
        for tag in ["synth_summary", "synth_concept", "adjudicate_contradiction"] {
            let err = adapter.generate(tag, "", "").unwrap_err();
            assert!(matches!(err, RouterError::Unavailable { .. }));
            assert!(err.is_fallback());
        }
    }

    #[test]
    fn fallback_extract_entities_returns_empty_list() {
        let adapter = FallbackAdapter::new();
        let out = adapter.generate("extract_entities", "", "").unwrap();
        assert!(out.contains("\"entities\""));
        assert!(out.contains("[]"));
    }
}
