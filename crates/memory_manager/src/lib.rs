//! `memory_manager` — decay state machine, retention scoring, working
//! memory, user memory CRUD, and the privacy-strip invariant.
//!
//! This crate implements the `memory_manager` module called out in
//! `docs/technical/architecture.md` §2.1. It is the on-device authority for the lifecycle of
//! every [`MemoryObject`]: how it transitions through the decay state
//! machine, how its retention score is computed and updated, how it
//! is pinned / unpinned / forgotten, and how it is bounded into a
//! [`WorkingMemory`] context window with TTL eviction.
//!
//! The crate is also the home of [`PrivacyStrip`] and
//! [`SynthesisOutput`] — the type-system-enforced invariant from
//! `docs/technical/design.md` §6 that "every synthesis output carries a
//! privacy strip describing its compute location, model, and egress".
//! It is impossible to construct a [`SynthesisOutput`] without first
//! constructing a [`PrivacyStrip`].
//!
//! Cross-references:
//!
//! * Memory model & decay: `docs/technical/design.md` §4.
//! * Decay state machine: `docs/technical/architecture.md` §7.
//! * Memory deliverables: `docs/technical/design.md` §4.

#![deny(missing_docs)]

// STABLE
pub mod channel_memory;
// STABLE
pub mod decay;
// STABLE
pub mod domain_memory;
// UNSTABLE — episodic memory; API still evolving.
pub mod episodic;
// STABLE
pub mod error;
// UNSTABLE — internal metrics; signatures may change.
#[doc(hidden)]
pub mod metrics;
// STABLE
pub mod object;
// STABLE
pub mod policy;
// STABLE
pub mod privacy_strip;
// STABLE
pub mod purge;
// STABLE
pub mod retention;
// STABLE
pub mod state;
// STABLE
pub mod tenant_memory;
// UNSTABLE — internal state-machine transitions; not part of consumer API.
#[doc(hidden)]
pub mod transitions;
// STABLE
pub mod user_memory;
// STABLE
pub mod working_memory;

// STABLE
pub use channel_memory::{
    ActiveTask, ChannelDecayReport, ChannelMemoryObject, Decision, OpenQuestion,
    DEFAULT_COMPLETED_TASK_TTL_DAYS, DEFAULT_RESOLVED_QUESTION_TTL_DAYS,
};
// STABLE
pub use decay::{decay_sweep, DecaySweepReport};
// STABLE
pub use domain_memory::{
    Dependency, DomainDecayReport, DomainMemoryObject, Procedure, Risk, Workstream,
    DEFAULT_COMPLETED_WORKSTREAM_TTL_DAYS, DEFAULT_RESOLVED_RISK_TTL_DAYS,
};
// STABLE
pub use error::{MemoryError, Result};
// STABLE
pub use object::{MemoryObject, SensitivityClass};
// STABLE
pub use policy::{PolicyDecision, PolicyEngine, PolicyScope, RetentionPolicy, ScopeRetentionState};
// STABLE
pub use privacy_strip::{ComputeLocation, PrivacyStrip, PrivacyStripBuilder, SynthesisOutput};
// STABLE
pub use purge::{
    purge_archived, purge_archived_default, PurgeConfig, PurgeReport,
    DEFAULT_ARCHIVED_RETENTION_DAYS,
};
// STABLE
pub use retention::{
    compute_retention_score, compute_with_profile, compute_with_weights_and_profile, DecayProfile,
    RetentionScore, RetentionWeights,
};
// STABLE
pub use state::MemoryState;
// STABLE
pub use tenant_memory::{
    ApprovedDocumentRef, CanonicalPolicy, ProductTaxonomyEntry, StableOrgKnowledge,
    TenantMemoryError, TenantMemoryObject,
};
// UNSTABLE — internal state-machine transitions; not part of consumer API.
#[doc(hidden)]
pub use transitions::MemoryStateMachine;
// STABLE
pub use user_memory::{MemoryFilter, UserMemoryObject};
// STABLE
pub use working_memory::{WorkingMemory, WorkingMemoryEntry};
