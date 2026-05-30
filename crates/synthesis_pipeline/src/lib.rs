//! `synthesis_pipeline` — channel / domain / tenant synthesis windows
//! and the encrypted synthesis-object publication contract.
//!
//! Per `ARCHITECTURE.md` §2.1, the synthesis pipeline owns:
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
//!   Bonsai-1.7B implementation lands when the SLM adapters are
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
//! * Module map: `ARCHITECTURE.md` §2.1.
//! * Synthesis hierarchy: `docs/DESIGN.md` §6.

#![deny(missing_docs)]

#[cfg(all(feature = "test-support", not(debug_assertions)))]
compile_error!("test-support must not be enabled in release builds");

pub mod election;
pub mod error;
pub mod hierarchy;
pub mod object;
pub mod pipeline;
pub mod publish;
pub mod schema;
pub mod window;

pub use election::{
    DeviceTier, ElectionCandidate, SynthesizerElection, SynthesizerRole, DEFAULT_BATTERY_FLOOR,
    DEFAULT_HEARTBEAT_TTL_SECS,
};
pub use error::{PipelineError, Result};
pub use hierarchy::{
    build_domain_summary_object, build_tenant_summary_object, open_domain_window,
    open_tenant_window, ApprovedDocument, ChannelOutput, DomainOutput, DomainSynthesisInput,
    HierarchyEnforcedWindowManager, TenantSynthesisInput, TieredWindowHandle, WindowScopeTier,
};
pub use object::{
    default_synthesis_object_version, ObjectId, SynthesisObject, SynthesisObjectType,
};
#[cfg(any(test, feature = "test-support"))]
pub use pipeline::NoOpSynthesizer;
pub use pipeline::{LlamaCppSynthesizer, SynthesisInputs, SynthesisPipeline};
pub use publish::{consume_synthesis_object, publish_synthesis_object, EncryptedSynthesisObject};
pub use schema::{
    EntityList, EntityRecord, EntityType, ImportanceTag, ImportanceTagClass, ObservationRow,
    ObservationRowKind, SummaryBundle,
};
pub use window::{SynthesisWindow, SynthesisWindowManager, WindowId, WindowStatus};
