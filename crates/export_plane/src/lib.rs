//! `export_plane` — portable concept profiles, export policies,
//! controls, and policy simulator for the Knowledge substrate.
//!
//! Per `docs/DESIGN.md` §3.5 (Export plane) and `ARCHITECTURE.md` §4.1
//! (Export Service), the substrate exposes a *narrow*, policy-gated
//! interface for moving curated knowledge out of the substrate into
//! external surfaces (LLM tools, downstream apps, integration
//! partners). The export plane never re-emits raw evidence by
//! default — it operates on canonical concepts and policy-approved
//! summaries, with an explicit audit trail for every render.
//!
//! This module ships:
//!
//! * Data model — [`profile`] module: [`profile::PortableConceptProfile`],
//!   [`profile::ApprovedConcept`], [`profile::ExportView`],
//!   [`profile::EvidencePack`], [`profile::ExportConstraint`].
//! * Policy engine — [`policy`] module: [`policy::ExportPolicy`],
//!   [`policy::PolicyEngine`], [`policy::ExportDecision`].
//! * Per-concept / summary / workflow controls and a registry —
//!   [`controls`] module.
//! * Concept approval workflow bridging `concept_graph` canonical
//!   nodes to export-plane approved concepts — [`approval`] module.
//! * Read-only policy simulator — [`simulator`] module.

#![deny(missing_docs)]

pub mod approval;
pub mod controls;
pub mod policy;
pub mod profile;
pub mod simulator;

pub use approval::{ApprovalError, ConceptApprovalWorkflow};
pub use controls::{
    ConceptExportControl, ExportControlError, ExportControlRegistry, RedactionLevel,
    SummaryExportControl, WorkflowExportControl,
};
pub use policy::{
    ExportDecision, ExportPolicy, ExportRejection, ExportRejectionReason, ExportViewError,
    ExportViewRequest, PolicyEngine,
};
pub use profile::{
    ApprovedConcept, ApprovedSummary, EvidencePack, ExportConstraint, ExportView,
    ExportViewContent, PortableConceptProfile, ReasoningRef,
};
pub use simulator::{PolicySimulator, SimulationResult};
