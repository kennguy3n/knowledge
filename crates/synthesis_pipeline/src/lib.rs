//! `synthesis_pipeline` — channel / domain / tenant synthesis windows
//! and the encrypted synthesis-object publication contract.
//!
//! Per `docs/technical/architecture.md` §2.1, the synthesis pipeline owns:
//!
//! * **Synthesis windows** — per-scope `(start, end)` pairs the
//!   synthesizer aggregates over (`Pending` / `InProgress` /
//!   `Complete` / `Failed`).
//! * **Synthesis objects** — typed payloads (episodic summary,
//!   channel recap, domain summary, tenant summary) emitted by the
//!   synthesizer once per scope window. Objects carry provenance and
//!   optional supersession pointers.
//! * **GBNF-shaped schema types** — the structured-output records
//!   the SLM is constrained to produce (importance tag, entity list,
//!   observation row, summary bundle).
//! * **`SynthesisPipeline` trait** — the synthesizer interface.
//!   Ships a `NoOpSynthesizer` test implementation; the on-device
//!   SLM implementation lands when the SLM adapters are
//!   wired up.
//! * **Elected-device role** — the small-group synthesizer protocol
//!   skeleton (`SynthesizerElection`, `SynthesizerRole`).
//! * **Encrypted publish/consume** — the `publish_synthesis_object`
//!   / `consume_synthesis_object` round-trip backed by the `crypto`
//!   crate's XChaCha20-Poly1305 AEAD with `(scope_id, window_id)`
//!   bound into the AAD.
//!
//! Cross-references:
//!
//! * Module map: `docs/technical/architecture.md` §2.1.
//! * Synthesis hierarchy: `docs/technical/design.md` §6.

#![deny(missing_docs)]

#[cfg(all(feature = "test-support", not(debug_assertions)))]
compile_error!("test-support must not be enabled in release builds");

// UNSTABLE — elected-device protocol; API may change.
pub mod election;
// STABLE
pub mod error;
// STABLE — offline synthesis-quality evaluation primitives.
pub mod eval;
// STABLE
pub mod hierarchy;
// STABLE — hybrid synthesis (NER extraction + SLM rephrasing).
#[cfg(feature = "hybrid-synthesis")]
pub mod hybrid;
// STABLE
pub mod metrics;
// STABLE
pub mod object;
// STABLE
pub mod pipeline;
// STABLE
pub mod quality;
// STABLE
pub mod publish;
// STABLE
pub mod schema;
// STABLE
pub mod window;

// UNSTABLE — elected-device protocol; API may change.
pub use election::{
    DeviceTier, ElectionCandidate, SynthesizerElection, SynthesizerRole,
    DEFAULT_BATTERY_DEFER_MEDIUM_FLOOR, DEFAULT_BATTERY_FLOOR, DEFAULT_HEARTBEAT_TTL_SECS,
};
// STABLE
pub use error::{PipelineError, Result};
// STABLE
pub use eval::{recap_in_language, term_coverage, ungrounded_recap_terms, Script, TermCoverage};
// STABLE
pub use hierarchy::{
    build_domain_summary_object, build_tenant_summary_object, open_domain_window,
    open_tenant_window, ApprovedDocument, ChannelOutput, DomainOutput, DomainSynthesisInput,
    HierarchyEnforcedWindowManager, TenantSynthesisInput, TieredWindowHandle, WindowScopeTier,
};
// STABLE
pub use metrics::{SynthesisMetrics, SynthesisMetricsSnapshot};
// STABLE
pub use object::{
    default_synthesis_object_version, ObjectId, SynthesisObject, SynthesisObjectType,
};
// STABLE
#[cfg(any(test, feature = "test-support"))]
pub use pipeline::NoOpSynthesizer;
// STABLE
pub use quality::{
    adaptive_budget, augment_recap_with_missing_entities, bundle_has_exemplar_token,
    retry_budget, salient_terms_from_texts, score_bundle, score_bundle_with_terms,
    strip_exemplar_leak, ungrounded_entry_count, verify_and_retry, Attempt, QualityReport,
    VerifiedSynthesis, RETRY_SUFFIX,
};
// STABLE
pub use pipeline::{LlamaCppSynthesizer, SynthesisInputs, SynthesisPipeline};
// STABLE — hybrid synthesizer (NER + SLM rephrase), gated behind
// the `hybrid-synthesis` feature.
#[cfg(feature = "hybrid-synthesis")]
pub use hybrid::HybridSynthesizer;
// STABLE
pub use publish::{consume_synthesis_object, publish_synthesis_object, EncryptedSynthesisObject};
// STABLE
pub use schema::{
    EntityList, EntityRecord, EntityType, ImportanceTag, ImportanceTagClass, ObservationRow,
    ObservationRowKind, SummaryBundle,
};
// STABLE
pub use window::{SynthesisWindow, SynthesisWindowManager, WindowId, WindowStatus};
