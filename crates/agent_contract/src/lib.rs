//! `agent_contract` — agent proposal-only write contract for the
//! Knowledge substrate.
//!
//! Per `docs/technical/design.md` §7.3, software agents (LLM-driven workflows,
//! integrations, AI employees) **never** write canonical memory
//! directly. Instead they speak to the substrate through a
//! proposal-only API:
//!
//! * `propose_observation(scope, claim, evidence_refs, …)`
//! * `propose_concept(scope, label, definition, evidence_refs, …)`
//! * `propose_relation(scope, src, type, dst, evidence_refs, …)`
//! * `propose_summary(scope, text, evidence_refs, …)`
//!
//! Promotion to canonical (`promote_to_canonical(proposal_id)`)
//! requires an explicit human action **or** a tenant policy that
//! auto-promotes proposals matching specific criteria (high
//! confidence, high cross-source corroboration, low sensitivity).
//!
//! This module ships:
//!
//! * The proposal data model — [`AgentProposal`], [`AgentIdentity`],
//!   and the four payload types ([`ObservationProposal`],
//!   [`ConceptProposal`], [`RelationProposal`], [`SummaryProposal`]).
//! * Schema validation — [`schema::validate_proposal`] and a
//!   [`schema::ProposalValidationError`] error type with specific
//!   variants per failure mode.
//! * Lifecycle state machine — [`lifecycle::ProposalState`],
//!   [`lifecycle::AutoPromotionPolicy`], [`lifecycle::ProposalStore`],
//!   and the canonical-promotion output type
//!   [`lifecycle::CanonicalArtifact`].
//!
//! Cross-references:
//!
//! * `docs/technical/design.md` §3.6 (Action plane), §7.3 (Agent write contract).
//! * `docs/technical/architecture.md` §6 (Permission model — `proposer` relation).
//! * `docs/technical/design.md` §3.6 (Agent contracts + export plane).

#![deny(missing_docs)]

// STABLE
pub mod lifecycle;
// STABLE
pub mod proposal;
// STABLE
pub mod schema;

// STABLE
pub use lifecycle::{
    AutoPromotionPolicy, CanonicalArtifact, CanonicalConcept, CanonicalObservation,
    CanonicalRelation, CanonicalSummary, ProposalDecision, ProposalState, ProposalStore,
    StoredProposal,
};
// STABLE
pub use proposal::{
    AgentIdentity, AgentProposal, ConceptProposal, ObservationProposal, ProposalKind,
    RelationProposal, RelationType, SummaryProposal,
};
// STABLE
pub use schema::{validate_proposal, ProposalValidationError};
