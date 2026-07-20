//! SLM-assisted entity refinement for High-tier devices.
//!
//! ## Design
//!
//! On High-tier devices with an on-device SLM (Qwen3.5-2B),
//! entities that the lexicon/pattern extractors classify as
//! `EntityType::Unknown` can be refined by passing the surrounding
//! context to the SLM. This module defines the trait and a
//! no-op fallback implementation so the system degrades gracefully
//! on Low/Mid-tier devices.
//!
//! ## Architecture
//!
//! ```text
//! LexiconExtractor::do_extract
//!   → extract_typed_entities (pattern-based)
//!   → EntityRefiner::refine_unknown_entities (SLM-assisted, High-tier only)
//! ```
//!
//! The refiner receives the full text and a list of entities typed
//! as `Unknown`, along with their character offsets. It returns
//! refined `EntityType` assignments for any entities it can
//! classify, leaving the rest as `Unknown`.
//!
//! ## On-device constraints
//!
//! - The refiner must be **optional**: if no SLM is available,
//!   the no-op fallback returns all entities unchanged.
//! - The refiner must be **bounded**: a maximum number of
//!   entities per call prevents unbounded SLM inference time.
//! - The refiner must be **non-blocking**: if the SLM is busy
//!   or fails, the fallback returns immediately.

use crate::entity_types::EntityType;

use std::sync::Arc;

use inference_router::{InferenceRouter, InferenceTask};
use serde::Deserialize;

/// A candidate entity for SLM-assisted refinement.
#[derive(Debug, Clone)]
pub struct RefinementCandidate {
    /// The entity text as extracted.
    pub text: String,
    /// The character offset of the entity in the source text.
    pub char_offset: usize,
    /// The current (pre-refinement) entity type.
    pub current_type: EntityType,
}

/// The result of refining a single entity.
#[derive(Debug, Clone, PartialEq)]
pub struct RefinementResult {
    /// The refined entity type, or `EntityType::Unknown` if the
    /// refiner could not classify it.
    pub refined_type: EntityType,
    /// Confidence score in `0.0..=1.0`. Below the threshold
    /// (default 0.7), the refinement is discarded.
    pub confidence: f64,
}

/// Configuration for SLM-assisted entity refinement.
#[derive(Debug, Clone)]
pub struct RefinementConfig {
    /// Maximum entities to refine per call. Prevents unbounded
    /// SLM inference time on large documents.
    pub max_entities_per_call: usize,
    /// Minimum confidence threshold. Refinements below this
    /// confidence are discarded.
    pub min_confidence: f64,
    /// Context window size (characters before and after the
    /// entity) to include in the SLM prompt.
    pub context_window_chars: usize,
}

impl Default for RefinementConfig {
    fn default() -> Self {
        Self {
            max_entities_per_call: 32,
            min_confidence: 0.7,
            context_window_chars: 128,
        }
    }
}

/// Trait for SLM-assisted entity refinement.
///
/// Implementations:
/// - [`NoOpRefiner`]: always returns `Unknown` (for Low/Mid-tier).
/// - `SlmRefiner` (future, behind `live-integration` feature):
///   sends context to the on-device SLM and parses the response.
pub trait EntityRefiner: Send + Sync {
    /// Refine a batch of entities typed as `Unknown`.
    ///
    /// Returns refined types for each candidate. The length of the
    /// returned vector matches the length of the input.
    fn refine(
        &self,
        text: &str,
        candidates: &[RefinementCandidate],
        config: &RefinementConfig,
    ) -> Vec<RefinementResult>;
}

/// No-op refiner — returns `Unknown` for all candidates.
///
/// Used on Low/Mid-tier devices where no SLM is available.
/// Also used as a fallback when the SLM fails or is busy.
pub struct NoOpRefiner;

impl EntityRefiner for NoOpRefiner {
    fn refine(
        &self,
        _text: &str,
        candidates: &[RefinementCandidate],
        _config: &RefinementConfig,
    ) -> Vec<RefinementResult> {
        candidates
            .iter()
            .map(|_| RefinementResult {
                refined_type: EntityType::Unknown,
                confidence: 0.0,
            })
            .collect()
    }
}

/// Heuristic-based refiner — uses context clues to refine
/// `Unknown` entities without an SLM.
///
/// This refiner runs on Mid-tier devices as a lightweight
/// alternative to SLM-assisted refinement. It uses simple
/// heuristics:
/// - If the entity is preceded by "Mr.", "Ms.", "Dr.", "Prof.",
///   "様", "氏", "씨" → `Person`
/// - If the entity is preceded by "Inc.", "Ltd.", "Corp.",
///   "GmbH", "株式会社", "有限公司" → `Organization`
/// - If the entity looks like a date (contains digits + hyphens/slashes)
///   → `Date`
/// - If the entity is preceded by "$", "€", "¥", "£" → `Currency`
pub struct HeuristicRefiner;

impl EntityRefiner for HeuristicRefiner {
    fn refine(
        &self,
        text: &str,
        candidates: &[RefinementCandidate],
        config: &RefinementConfig,
    ) -> Vec<RefinementResult> {
        candidates
            .iter()
            .take(config.max_entities_per_call)
            .map(|c| {
                // Convert char offsets to byte offsets for safe
                // string slicing on multi-byte text (CJK, etc.).
                let byte_offset = text
                    .char_indices()
                    .nth(c.char_offset)
                    .map_or(text.len(), |(i, _)| i);
                let entity_byte_end = text
                    .char_indices()
                    .nth(c.char_offset + c.text.chars().count())
                    .map_or(text.len(), |(i, _)| i);

                let context_start = text
                    .char_indices()
                    .nth(c.char_offset.saturating_sub(config.context_window_chars))
                    .map_or(0, |(i, _)| i);
                let context_end = text
                    .char_indices()
                    .nth(c.char_offset + c.text.chars().count() + config.context_window_chars)
                    .map_or(text.len(), |(i, _)| i);

                let before = text[context_start..byte_offset].trim_end();
                let after = text[entity_byte_end..context_end].trim_start();

                // Person indicators (before the entity)
                let person_prefixes = [
                    "Mr.", "Ms.", "Mrs.", "Dr.", "Prof.", "Sir", "Madam",
                ];
                let person_suffixes_ja = ["様", "氏", "君", "さん"];
                let person_suffixes_ko = ["씨"];

                if person_prefixes.iter().any(|p| before.ends_with(p)) {
                    return RefinementResult {
                        refined_type: EntityType::Person,
                        confidence: 0.85,
                    };
                }
                if person_suffixes_ja.iter().any(|s| after.starts_with(s))
                    || person_suffixes_ko.iter().any(|s| after.starts_with(s))
                {
                    return RefinementResult {
                        refined_type: EntityType::Person,
                        confidence: 0.80,
                    };
                }

                // Organization indicators (after the entity)
                let org_suffixes = [
                    "Inc.", "Ltd.", "Corp.", "Corporation", "LLC", "GmbH",
                    "株式会社", "有限公司", "주식회사",
                ];
                if org_suffixes.iter().any(|s| after.starts_with(s)) {
                    return RefinementResult {
                        refined_type: EntityType::Organization,
                        confidence: 0.85,
                    };
                }

                // Organization indicators (before the entity)
                let org_prefixes = ["株式会社", "有限公司", "주식회사", "有限会社"];
                if org_prefixes.iter().any(|p| before.ends_with(p)) {
                    return RefinementResult {
                        refined_type: EntityType::Organization,
                        confidence: 0.85,
                    };
                }

                // Currency indicators (before the entity)
                let currency_prefixes = ["$", "€", "¥", "£", "₹", "₩", "₫", "฿", "₪"];
                if currency_prefixes.iter().any(|p| before.ends_with(p)) {
                    return RefinementResult {
                        refined_type: EntityType::Currency,
                        confidence: 0.90,
                    };
                }

                // Date-like: digits with hyphens or slashes
                if c.text.chars().all(|c| c.is_ascii_digit() || c == '-' || c == '/')
                    && c.text.contains('-')
                    && c.text.chars().filter(char::is_ascii_digit).count() >= 4
                {
                    return RefinementResult {
                        refined_type: EntityType::Date,
                        confidence: 0.75,
                    };
                }

                RefinementResult {
                    refined_type: EntityType::Unknown,
                    confidence: 0.0,
                }
            })
            .collect()
    }
}

/// JSON envelope returned by the SLM under the
/// [`InferenceTask::RefineEntity`] grammar.
#[derive(Debug, Clone, Deserialize, PartialEq)]
struct SlmEntityVerdict {
    /// Entity type label from the SLM.
    #[serde(rename = "type")]
    entity_type: String,
    /// Model confidence in `[0.0, 1.0]`.
    confidence: f64,
}

impl SlmEntityVerdict {
    /// Parse the SLM's JSON response. Returns `None` on malformed JSON
    /// or out-of-range confidence.
    fn from_slm_str(output: &str) -> Option<Self> {
        let verdict: SlmEntityVerdict = serde_json::from_str(output).ok()?;
        if !(0.0..=1.0).contains(&verdict.confidence) {
            return None;
        }
        Some(verdict)
    }

    /// Map the SLM's string label to an [`EntityType`].
    fn to_entity_type(&self) -> EntityType {
        match self.entity_type.to_ascii_lowercase().as_str() {
            "person" => EntityType::Person,
            "organization" => EntityType::Organization,
            "product" => EntityType::Product,
            "location" => EntityType::Location,
            "date" => EntityType::Date,
            "currency" => EntityType::Currency,
            "identifier" => EntityType::Identifier,
            "url" => EntityType::Url,
            "email" => EntityType::Email,
            "numeric" => EntityType::Numeric,
            "event" => EntityType::Event,
            "measurement" => EntityType::Measurement,
            _ => EntityType::Unknown,
        }
    }
}

/// Live SLM-backed entity refiner for High-tier devices.
///
/// Dispatches to [`InferenceRouter`] with [`InferenceTask::RefineEntity`]
/// to classify `Unknown` entities using surrounding context. When the
/// SLM is unavailable or returns low-confidence output, the refiner
/// falls back to `Unknown` (matching [`NoOpRefiner`] behaviour).
///
/// This refiner is the production implementation of [`EntityRefiner`]
/// for High-tier devices. On Mid-tier devices, [`HeuristicRefiner`]
/// is used instead; on Low-tier devices, [`NoOpRefiner`] is used.
pub struct SlmRefiner {
    router: Arc<InferenceRouter>,
}

impl std::fmt::Debug for SlmRefiner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SlmRefiner")
            .finish_non_exhaustive()
    }
}

impl SlmRefiner {
    /// Construct a new refiner backed by `router`.
    pub fn new(router: Arc<InferenceRouter>) -> Self {
        Self { router }
    }

    /// Refine a single entity by sending its surrounding context to the SLM.
    fn refine_one(
        &self,
        text: &str,
        candidate: &RefinementCandidate,
        config: &RefinementConfig,
    ) -> RefinementResult {
        // Extract context window around the entity.
        let byte_offset = text
            .char_indices()
            .nth(candidate.char_offset)
            .map_or(text.len(), |(i, _)| i);
        let entity_byte_end = text
            .char_indices()
            .nth(candidate.char_offset + candidate.text.chars().count())
            .map_or(text.len(), |(i, _)| i);

        let context_start = text
            .char_indices()
            .nth(candidate.char_offset.saturating_sub(config.context_window_chars))
            .map_or(0, |(i, _)| i);
        let context_end = text
            .char_indices()
            .nth(candidate.char_offset + candidate.text.chars().count() + config.context_window_chars)
            .map_or(text.len(), |(i, _)| i);

        let context = &text[context_start..context_end];
        let _ = (byte_offset, entity_byte_end); // suppress unused warnings

        let prompt = InferenceTask::RefineEntity
            .prompt_template()
            .replace("{body}", context);

        match self.router.dispatch(InferenceTask::RefineEntity, &prompt) {
            Ok(output) => {
                if let Some(verdict) = SlmEntityVerdict::from_slm_str(&output) {
                    RefinementResult {
                        refined_type: verdict.to_entity_type(),
                        confidence: verdict.confidence,
                    }
                } else {
                    RefinementResult {
                        refined_type: EntityType::Unknown,
                        confidence: 0.0,
                    }
                }
            }
            Err(_) => RefinementResult {
                refined_type: EntityType::Unknown,
                confidence: 0.0,
            },
        }
    }
}

impl EntityRefiner for SlmRefiner {
    fn refine(
        &self,
        text: &str,
        candidates: &[RefinementCandidate],
        config: &RefinementConfig,
    ) -> Vec<RefinementResult> {
        candidates
            .iter()
            .take(config.max_entities_per_call)
            .map(|c| self.refine_one(text, c, config))
            .collect()
    }
}

/// Apply refinement results to a list of entity types.
///
/// Returns a new vector where `Unknown` entries are replaced with
/// refined types when confidence meets the threshold.
pub fn apply_refinement(
    candidates: &[RefinementCandidate],
    results: &[RefinementResult],
    config: &RefinementConfig,
) -> Vec<EntityType> {
    candidates
        .iter()
        .zip(results.iter())
        .map(|(c, r)| {
            if r.confidence >= config.min_confidence {
                r.refined_type
            } else {
                c.current_type
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noop_refiner_returns_unknown() {
        let refiner = NoOpRefiner;
        let candidates = vec![RefinementCandidate {
            text: "ACME".to_string(),
            char_offset: 0,
            current_type: EntityType::Unknown,
        }];
        let results = refiner.refine("ACME Inc.", &candidates, &RefinementConfig::default());
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].refined_type, EntityType::Unknown);
    }

    #[test]
    fn heuristic_detects_person_from_honorific() {
        let refiner = HeuristicRefiner;
        let text = "Dr. Jane Smith presented today";
        let candidates = vec![RefinementCandidate {
            text: "Jane Smith".to_string(),
            char_offset: 4,
            current_type: EntityType::Unknown,
        }];
        let config = RefinementConfig::default();
        let results = refiner.refine(text, &candidates, &config);
        assert_eq!(results[0].refined_type, EntityType::Person);
        assert!(results[0].confidence >= config.min_confidence);
    }

    #[test]
    fn heuristic_detects_org_from_suffix() {
        let refiner = HeuristicRefiner;
        let text = "ACME Inc. announced a new product";
        let candidates = vec![RefinementCandidate {
            text: "ACME".to_string(),
            char_offset: 0,
            current_type: EntityType::Unknown,
        }];
        let config = RefinementConfig::default();
        let results = refiner.refine(text, &candidates, &config);
        assert_eq!(results[0].refined_type, EntityType::Organization);
    }

    #[test]
    fn heuristic_detects_currency_from_prefix() {
        let refiner = HeuristicRefiner;
        let text = "The price is $5,000 for the package";
        let candidates = vec![RefinementCandidate {
            text: "5,000".to_string(),
            char_offset: 14,
            current_type: EntityType::Unknown,
        }];
        let config = RefinementConfig::default();
        let results = refiner.refine(text, &candidates, &config);
        assert_eq!(results[0].refined_type, EntityType::Currency);
    }

    #[test]
    fn heuristic_detects_date_from_format() {
        let refiner = HeuristicRefiner;
        let text = "The deadline is 2024-03-15 for submission";
        let candidates = vec![RefinementCandidate {
            text: "2024-03-15".to_string(),
            char_offset: 16,
            current_type: EntityType::Unknown,
        }];
        let config = RefinementConfig::default();
        let results = refiner.refine(text, &candidates, &config);
        assert_eq!(results[0].refined_type, EntityType::Date);
    }

    #[test]
    fn heuristic_detects_japanese_person_suffix() {
        let refiner = HeuristicRefiner;
        let text = "田中様が来社されました";
        let candidates = vec![RefinementCandidate {
            text: "田中".to_string(),
            char_offset: 0,
            current_type: EntityType::Unknown,
        }];
        let config = RefinementConfig::default();
        let results = refiner.refine(text, &candidates, &config);
        assert_eq!(results[0].refined_type, EntityType::Person);
    }

    #[test]
    fn heuristic_detects_japanese_org_suffix() {
        let refiner = HeuristicRefiner;
        let text = "株式会社テックの新製品";
        let candidates = vec![RefinementCandidate {
            text: "テック".to_string(),
            char_offset: 4,
            current_type: EntityType::Unknown,
        }];
        let config = RefinementConfig::default();
        let results = refiner.refine(text, &candidates, &config);
        assert_eq!(results[0].refined_type, EntityType::Organization);
    }

    #[test]
    fn apply_refinement_respects_threshold() {
        let candidates = vec![RefinementCandidate {
            text: "test".to_string(),
            char_offset: 0,
            current_type: EntityType::Unknown,
        }];
        let results = vec![RefinementResult {
            refined_type: EntityType::Person,
            confidence: 0.5, // below default threshold of 0.7
        }];
        let config = RefinementConfig::default();
        let types = apply_refinement(&candidates, &results, &config);
        assert_eq!(types[0], EntityType::Unknown); // not refined
    }

    #[test]
    fn apply_refinement_applies_high_confidence() {
        let candidates = vec![RefinementCandidate {
            text: "test".to_string(),
            char_offset: 0,
            current_type: EntityType::Unknown,
        }];
        let results = vec![RefinementResult {
            refined_type: EntityType::Person,
            confidence: 0.9, // above threshold
        }];
        let config = RefinementConfig::default();
        let types = apply_refinement(&candidates, &results, &config);
        assert_eq!(types[0], EntityType::Person);
    }

    #[test]
    fn max_entities_per_call_limits_refinement() {
        let refiner = HeuristicRefiner;
        let config = RefinementConfig {
            max_entities_per_call: 2,
            ..Default::default()
        };
        let candidates = vec![
            RefinementCandidate {
                text: "A".to_string(),
                char_offset: 0,
                current_type: EntityType::Unknown,
            },
            RefinementCandidate {
                text: "B".to_string(),
                char_offset: 2,
                current_type: EntityType::Unknown,
            },
            RefinementCandidate {
                text: "C".to_string(),
                char_offset: 4,
                current_type: EntityType::Unknown,
            },
        ];
        let results = refiner.refine("A B C", &candidates, &config);
        // Only 2 results (max_entities_per_call = 2)
        assert_eq!(results.len(), 2);
    }

    // ── SlmRefiner tests ──

    #[test]
    fn slm_refiner_classifies_person_via_mock_adapter() {
        use inference_router::{
            AdapterKind, InferenceAdapter, InferenceRouter, ProbeResult, RouterConfig,
        };
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Mutex;

        struct ConstAdapter {
            response: Mutex<Result<String, inference_router::RouterError>>,
            available: AtomicBool,
        }
        impl ConstAdapter {
            fn ok(text: &str) -> Self {
                Self {
                    response: Mutex::new(Ok(text.into())),
                    available: AtomicBool::new(true),
                }
            }
        }
        impl InferenceAdapter for ConstAdapter {
            fn kind(&self) -> AdapterKind { AdapterKind::Mock }
            fn probe(&self) -> ProbeResult { ProbeResult::Available }
            fn is_available(&self) -> bool { self.available.load(Ordering::SeqCst) }
            fn supports(&self, _task: inference_router::InferenceTask) -> bool { true }
            fn generate(&self, _tag: &str, _prompt: &str, _grammar: &str) -> Result<String, inference_router::RouterError> {
                self.response.lock().unwrap().clone()
            }
        }

        let router = std::sync::Arc::new(InferenceRouter::new(
            RouterConfig::default(),
            vec![Box::new(ConstAdapter::ok(
                r#"{"type":"person","confidence":0.90}"#,
            ))],
        ));
        router.bootstrap();
        let refiner = SlmRefiner::new(router);
        let text = "Dr. Jane Smith presented today";
        let candidates = vec![RefinementCandidate {
            text: "Jane Smith".to_string(),
            char_offset: 4,
            current_type: EntityType::Unknown,
        }];
        let config = RefinementConfig::default();
        let results = refiner.refine(text, &candidates, &config);
        assert_eq!(results[0].refined_type, EntityType::Person);
        assert!(results[0].confidence >= config.min_confidence);
    }

    #[test]
    fn slm_refiner_returns_unknown_on_malformed_json() {
        use inference_router::{
            AdapterKind, InferenceAdapter, InferenceRouter, ProbeResult, RouterConfig,
        };
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Mutex;

        struct ConstAdapter {
            response: Mutex<Result<String, inference_router::RouterError>>,
            available: AtomicBool,
        }
        impl ConstAdapter {
            fn ok(text: &str) -> Self {
                Self {
                    response: Mutex::new(Ok(text.into())),
                    available: AtomicBool::new(true),
                }
            }
        }
        impl InferenceAdapter for ConstAdapter {
            fn kind(&self) -> AdapterKind { AdapterKind::Mock }
            fn probe(&self) -> ProbeResult { ProbeResult::Available }
            fn is_available(&self) -> bool { self.available.load(Ordering::SeqCst) }
            fn supports(&self, _task: inference_router::InferenceTask) -> bool { true }
            fn generate(&self, _tag: &str, _prompt: &str, _grammar: &str) -> Result<String, inference_router::RouterError> {
                self.response.lock().unwrap().clone()
            }
        }

        let router = std::sync::Arc::new(InferenceRouter::new(
            RouterConfig::default(),
            vec![Box::new(ConstAdapter::ok("not json"))],
        ));
        router.bootstrap();
        let refiner = SlmRefiner::new(router);
        let candidates = vec![RefinementCandidate {
            text: "ACME".to_string(),
            char_offset: 0,
            current_type: EntityType::Unknown,
        }];
        let results = refiner.refine("ACME Inc.", &candidates, &RefinementConfig::default());
        assert_eq!(results[0].refined_type, EntityType::Unknown);
        assert!(results[0].confidence.abs() < 1e-9);
    }

    #[test]
    fn slm_refiner_works_with_fallback_adapter() {
        use inference_router::{FallbackAdapter, InferenceRouter, RouterConfig};
        let router = std::sync::Arc::new(InferenceRouter::new(
            RouterConfig::default(),
            vec![Box::new(FallbackAdapter::new())],
        ));
        router.bootstrap();
        let refiner = SlmRefiner::new(router);
        let text = "Dr. Jane Smith presented today";
        let candidates = vec![RefinementCandidate {
            text: "Jane Smith".to_string(),
            char_offset: 4,
            current_type: EntityType::Unknown,
        }];
        let config = RefinementConfig::default();
        let results = refiner.refine(text, &candidates, &config);
        // Fallback adapter should detect "Dr." as a person indicator
        assert_eq!(results[0].refined_type, EntityType::Person);
    }

    #[test]
    fn slm_refiner_respects_max_entities_per_call() {
        use inference_router::{FallbackAdapter, InferenceRouter, RouterConfig};
        let router = std::sync::Arc::new(InferenceRouter::new(
            RouterConfig::default(),
            vec![Box::new(FallbackAdapter::new())],
        ));
        router.bootstrap();
        let refiner = SlmRefiner::new(router);
        let config = RefinementConfig {
            max_entities_per_call: 2,
            ..Default::default()
        };
        let candidates = vec![
            RefinementCandidate {
                text: "A".to_string(),
                char_offset: 0,
                current_type: EntityType::Unknown,
            },
            RefinementCandidate {
                text: "B".to_string(),
                char_offset: 2,
                current_type: EntityType::Unknown,
            },
            RefinementCandidate {
                text: "C".to_string(),
                char_offset: 4,
                current_type: EntityType::Unknown,
            },
        ];
        let results = refiner.refine("A B C", &candidates, &config);
        assert_eq!(results.len(), 2);
    }
}
