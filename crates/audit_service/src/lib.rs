//! `audit_service` — append-only audit log for the Knowledge substrate.
//!
//! Per `ARCHITECTURE.md` §4.1, the audit service records an
//! **append-only audit log of canonical promotions, exports, agent
//! proposals, policy changes**, plus tenant-lifecycle events
//! (provisioning, deletion, key destruction).
//!
//! The Phase 3 implementation is a deterministic in-memory log keyed
//! by [`AuditEntryId`] and ordered by [`AuditEntry::sequence`]. The
//! log is *append-only* — there is no public mutate / delete API; the
//! type system enforces that an inserted entry cannot be modified or
//! removed. Persistence (Postgres / object-store) lands in later
//! phases.

#![deny(missing_docs)]

pub mod entry;
pub mod error;
pub mod helpers;
pub mod log;

pub use entry::{
    Actor, AuditActionType, AuditEntry, AuditEntryBuilder, AuditEntryId, TargetRef, TargetType,
};
pub use error::{AuditError, Result};
pub use helpers::{
    log_export, log_export_simulated, log_proposal_promoted, log_proposal_rejected,
    log_proposal_submitted,
};
pub use log::{AuditLog, AuditQuery};
