//! SLM-backed and composite importance classifiers.
//!
//! `crate::importance::LexiconClassifier` is the deterministic
//! fallback. This module wires the substrate to the on-device SLM via
//! [`inference_router::InferenceRouter`] and adds a
//! [`CompositeClassifier`] that runs the lexicon first as a cheap
//! pre-filter and dispatches to the SLM only when the lexicon is
//! uncertain.

use std::sync::Arc;

use inference_router::{InferenceRouter, InferenceTask};
use serde::{Deserialize, Serialize};

use crate::importance::{ImportanceClass, ImportanceClassifier, LexiconClassifier};

/// JSON envelope returned by the SLM under the
/// [`InferenceTask::TagImportance`] grammar.
///
/// The grammar in `inference_router::task` constrains the model to
/// emit exactly:
/// `{"class": "critical|important|useful|noise", "confidence": 0.0-1.0}`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SlmImportanceVerdict {
    /// Importance class.
    pub class: String,
    /// Model confidence in `[0.0, 1.0]`.
    pub confidence: f64,
}

impl SlmImportanceVerdict {
    /// Convert the lower-cased `class` field into a typed
    /// [`ImportanceClass`]. Unknown labels return `None`.
    pub fn class(&self) -> Option<ImportanceClass> {
        match self.class.to_ascii_lowercase().as_str() {
            "critical" => Some(ImportanceClass::Critical),
            "important" => Some(ImportanceClass::Important),
            "useful" => Some(ImportanceClass::Useful),
            "noise" => Some(ImportanceClass::Noise),
            _ => None,
        }
    }
}

/// SLM-backed importance classifier. Dispatches to
/// [`InferenceRouter`] with the [`InferenceTask::TagImportance`]
/// task; falls back to the wrapped [`LexiconClassifier`] when the
/// router signals fallback (`Unavailable` / `TierTooLow`) or returns
/// an output that fails grammar validation.
pub struct SlmClassifier {
    router: Arc<InferenceRouter>,
    fallback: LexiconClassifier,
}

impl std::fmt::Debug for SlmClassifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SlmClassifier")
            .field("fallback", &self.fallback)
            .finish_non_exhaustive()
    }
}

impl SlmClassifier {
    /// Construct a new classifier backed by `router`. Uses the
    /// substrate's default English lexicon as the fallback.
    pub fn new(router: Arc<InferenceRouter>) -> Self {
        Self {
            router,
            fallback: LexiconClassifier::english_default(),
        }
    }

    /// Construct a classifier with an explicit fallback lexicon
    /// classifier — useful for per-tenant lexicons.
    pub fn with_fallback(router: Arc<InferenceRouter>, fallback: LexiconClassifier) -> Self {
        Self { router, fallback }
    }

    /// Run the SLM call and parse the JSON verdict. On any
    /// fallback-class router error, fall through to the lexicon
    /// classifier.
    fn classify_via_slm(&self, text: &str) -> ImportanceClass {
        let prompt = render_prompt(text);
        match self.router.dispatch(InferenceTask::TagImportance, &prompt) {
            Ok(output) => match parse_verdict(&output) {
                Some(class) => class,
                None => self.fallback.classify(text),
            },
            Err(err) if err.is_fallback() => self.fallback.classify(text),
            // Inference failure (network, model crash, grammar
            // violation that the runtime caught) — also fall back so
            // the substrate keeps moving rather than dropping the
            // evidence row.
            Err(_) => self.fallback.classify(text),
        }
    }
}

impl ImportanceClassifier for SlmClassifier {
    fn classify(&self, text: &str) -> ImportanceClass {
        self.classify_via_slm(text)
    }
}

/// Composite classifier — chains the lexicon and the SLM.
///
/// The lexicon runs first as a cheap pre-filter. If the lexicon
/// returns a "high-confidence" class (`Noise` or `Critical` — the
/// extremes the lexicon is best at) the SLM is skipped entirely. For
/// the uncertain middle (`Useful` / `Important`) the SLM is
/// dispatched and may upgrade or downgrade the verdict.
pub struct CompositeClassifier {
    lexicon: LexiconClassifier,
    slm: Option<SlmClassifier>,
}

impl std::fmt::Debug for CompositeClassifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompositeClassifier")
            .field("lexicon", &self.lexicon)
            .field("has_slm", &self.slm.is_some())
            .finish()
    }
}

impl CompositeClassifier {
    /// Build a composite classifier from a lexicon and an SLM
    /// classifier.
    pub fn new(lexicon: LexiconClassifier, slm: SlmClassifier) -> Self {
        Self {
            lexicon,
            slm: Some(slm),
        }
    }

    /// Build a composite without an SLM stage — degrades to plain
    /// lexicon classification. Used on Low-tier devices.
    pub fn lexicon_only(lexicon: LexiconClassifier) -> Self {
        Self { lexicon, slm: None }
    }

    /// `true` iff the SLM stage is plumbed in.
    pub fn has_slm(&self) -> bool {
        self.slm.is_some()
    }

    fn lexicon_is_high_confidence(class: ImportanceClass) -> bool {
        matches!(class, ImportanceClass::Noise | ImportanceClass::Critical)
    }
}

impl ImportanceClassifier for CompositeClassifier {
    fn classify(&self, text: &str) -> ImportanceClass {
        let lexicon_class = self.lexicon.classify(text);
        if Self::lexicon_is_high_confidence(lexicon_class) {
            return lexicon_class;
        }
        match &self.slm {
            Some(slm) => slm.classify(text),
            None => lexicon_class,
        }
    }
}

/// Render the [`InferenceTask::TagImportance`] prompt with `body`
/// substituted into the template.
pub fn render_prompt(body: &str) -> String {
    InferenceTask::TagImportance
        .prompt_template()
        .replace("{body}", body)
}

/// Parse the SLM's JSON response into an [`ImportanceClass`].
/// Returns `None` when the JSON is missing, malformed, or carries an
/// unknown class label.
pub fn parse_verdict(output: &str) -> Option<ImportanceClass> {
    let verdict: SlmImportanceVerdict = serde_json::from_str(output).ok()?;
    if !(0.0..=1.0).contains(&verdict.confidence) {
        return None;
    }
    verdict.class()
}

/// `true` iff `output` parses as a well-formed [`SlmImportanceVerdict`].
pub fn validate_grammar(output: &str) -> bool {
    parse_verdict(output).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use inference_router::{
        AdapterKind, FallbackAdapter, InferenceAdapter, InferenceRouter, ProbeResult, RouterConfig,
        RouterError,
    };
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;

    /// Mock adapter with a fixed response so the unit tests stay
    /// hermetic.
    struct ConstAdapter {
        response: Mutex<Result<String, RouterError>>,
        available: AtomicBool,
    }

    impl ConstAdapter {
        fn ok(text: &str) -> Self {
            Self {
                response: Mutex::new(Ok(text.into())),
                available: AtomicBool::new(true),
            }
        }
        fn err(err: RouterError) -> Self {
            Self {
                response: Mutex::new(Err(err)),
                available: AtomicBool::new(true),
            }
        }
    }

    impl InferenceAdapter for ConstAdapter {
        fn kind(&self) -> AdapterKind {
            AdapterKind::Mock
        }
        fn probe(&self) -> ProbeResult {
            ProbeResult::Available
        }
        fn is_available(&self) -> bool {
            self.available.load(Ordering::SeqCst)
        }
        fn supports(&self, _task: InferenceTask) -> bool {
            true
        }
        fn generate(&self,
            _task_tag: &str,
            _prompt: &str,
            _grammar: &str,
        ) -> Result<String, RouterError> {
            self.response.lock().unwrap().clone()
        }
    }

    fn router_with(adapter: Box<dyn InferenceAdapter>) -> Arc<InferenceRouter> {
        let config = RouterConfig::default();
        let router = InferenceRouter::new(config, vec![adapter]);
        router.bootstrap();
        Arc::new(router)
    }

    #[test]
    fn slm_classifier_parses_critical_verdict() {
        let router = router_with(Box::new(ConstAdapter::ok(r#"{"class":"critical","confidence":0.9}"#,
        )));
        let c = SlmClassifier::new(router);
        assert_eq!(c.classify("the regulator demands a 24h response"),
            ImportanceClass::Critical
        );
    }

    #[test]
    fn slm_classifier_parses_useful_verdict() {
        let router = router_with(Box::new(ConstAdapter::ok(r#"{"class":"useful","confidence":0.4}"#,
        )));
        let c = SlmClassifier::new(router);
        assert_eq!(c.classify("any text here"), ImportanceClass::Useful);
    }

    #[test]
    fn slm_classifier_falls_back_on_router_unavailable() {
        let router = router_with(Box::new(ConstAdapter::err(RouterError::Unavailable {
            task: "tag_importance",
        })));
        let c = SlmClassifier::new(router);
        // Lexicon would mark this as critical via `compliance` keyword.
        assert_eq!(c.classify("compliance review needed"),
            ImportanceClass::Critical
        );
    }

    #[test]
    fn slm_classifier_falls_back_on_inference_failure() {
        let router = router_with(Box::new(ConstAdapter::err(RouterError::InferenceFailure("network down".into(),
        ))));
        let c = SlmClassifier::new(router);
        assert_eq!(c.classify("the deadline is tomorrow"),
            ImportanceClass::Important
        );
    }

    #[test]
    fn slm_classifier_falls_back_on_malformed_json() {
        let router = router_with(Box::new(ConstAdapter::ok("not-json")));
        let c = SlmClassifier::new(router);
        assert_eq!(c.classify("the deadline is tomorrow"),
            ImportanceClass::Important
        );
    }

    #[test]
    fn slm_classifier_rejects_out_of_range_confidence() {
        let router = router_with(Box::new(ConstAdapter::ok(r#"{"class":"critical","confidence":42.0}"#,
        )));
        let c = SlmClassifier::new(router);
        // 42.0 is invalid → fall back to lexicon. Without lexicon-level
        // critical/important keywords the lexicon settles on Useful.
        assert_eq!(c.classify("the team should review this in detail"),
            ImportanceClass::Useful
        );
    }

    #[test]
    fn slm_classifier_rejects_unknown_class_label() {
        let router = router_with(Box::new(ConstAdapter::ok(r#"{"class":"super-critical","confidence":0.9}"#,
        )));
        let c = SlmClassifier::new(router);
        assert_eq!(c.classify("compliance review needed"),
            ImportanceClass::Critical // via lexicon fallback
        );
    }

    #[test]
    fn composite_short_circuits_on_lexicon_noise() {
        // SLM would have said Critical; composite must keep Noise.
        let router = router_with(Box::new(ConstAdapter::ok(r#"{"class":"critical","confidence":0.99}"#,
        )));
        let comp = CompositeClassifier::new(LexiconClassifier::english_default(),
            SlmClassifier::new(router),
        );
        assert_eq!(comp.classify("hi"), ImportanceClass::Noise);
    }

    #[test]
    fn composite_short_circuits_on_lexicon_critical() {
        // SLM would have said Useful; composite must keep Critical.
        let router = router_with(Box::new(ConstAdapter::ok(r#"{"class":"useful","confidence":0.4}"#,
        )));
        let comp = CompositeClassifier::new(LexiconClassifier::english_default(),
            SlmClassifier::new(router),
        );
        assert_eq!(comp.classify("Legal hold issued on the marketing channel."),
            ImportanceClass::Critical
        );
    }

    #[test]
    fn composite_dispatches_to_slm_on_useful() {
        // Lexicon would classify "let's revisit the dashboard" as
        // Useful. The SLM upgrades that to Important, and the
        // composite must use the SLM verdict.
        let router = router_with(Box::new(ConstAdapter::ok(r#"{"class":"important","confidence":0.7}"#,
        )));
        let comp = CompositeClassifier::new(LexiconClassifier::english_default(),
            SlmClassifier::new(router),
        );
        assert_eq!(comp.classify("let's revisit the dashboard tomorrow"),
            ImportanceClass::Important
        );
    }

    #[test]
    fn composite_lexicon_only_when_slm_is_absent() {
        let comp = CompositeClassifier::lexicon_only(LexiconClassifier::english_default());
        assert!(!comp.has_slm());
        assert_eq!(comp.classify("Friday is the deadline for the migration."),
            ImportanceClass::Important
        );
    }

    #[test]
    fn composite_falls_through_to_lexicon_when_slm_unavailable() {
        // Useful (uncertain) → SLM fires → router unavailable → SLM
        // falls back to lexicon. End-to-end: lexicon's verdict wins.
        let router = router_with(Box::new(ConstAdapter::err(RouterError::Unavailable {
            task: "tag_importance",
        })));
        let comp = CompositeClassifier::new(LexiconClassifier::english_default(),
            SlmClassifier::new(router),
        );
        assert_eq!(comp.classify("let's revisit the dashboard tomorrow"),
            ImportanceClass::Useful
        );
    }

    #[test]
    fn render_prompt_substitutes_body_placeholder() {
        let prompt = render_prompt("hello world");
        assert!(prompt.contains("hello world"));
        assert!(!prompt.contains("{body}"));
    }

    #[test]
    fn validate_grammar_accepts_well_formed_json() {
        assert!(validate_grammar(r#"{"class":"useful","confidence":0.5}"#));
    }

    #[test]
    fn validate_grammar_rejects_malformed_or_out_of_range() {
        assert!(!validate_grammar("not-json"));
        assert!(!validate_grammar(r#"{"class":"useful","confidence":2.0}"#));
        assert!(!validate_grammar(r#"{"class":"unknown","confidence":0.5}"#));
    }

    #[test]
    fn slm_classifier_integration_with_real_fallback_adapter() {
        // The real FallbackAdapter scores against a lexicon; the
        // classifier exists to lift its verdict into
        // `ImportanceClass`. Feed it bodies that exercise each class
        // and assert the round-trip mapping.
        let cfg = RouterConfig::default();
        let router = Arc::new(InferenceRouter::new(cfg,
            vec![Box::new(FallbackAdapter::new())],
        ));
        router.bootstrap();
        let c = SlmClassifier::new(router);

        // "Critical" lexicon term.
        assert_eq!(c.classify("Security incident in production — please page on-call"),
            ImportanceClass::Critical
        );
        // "Important" lexicon term.
        assert_eq!(c.classify("Please review the deadline for the launch"),
            ImportanceClass::Important
        );
        // "Useful" lexicon term (question / interrogative).
        assert_eq!(c.classify("Could you investigate the question on routing?"),
            ImportanceClass::Useful
        );
        // No signal at all → noise class.
        assert_eq!(c.classify("any random body without keywords"),
            ImportanceClass::Noise
        );
    }
}
