//! `audit_service` — append-only audit log for the Knowledge substrate.
//!
//! Per `ARCHITECTURE.md` §4.1, the audit service records an
//! **append-only audit log of canonical promotions, exports, agent
//! proposals, policy changes**, plus tenant-lifecycle events
//! (provisioning, deletion, key destruction).
//!
//! The crate exposes two layered logs:
//!
//! * [`AuditLog`] — an in-memory append-only log keyed by
//!   [`AuditEntryId`] and ordered by [`AuditEntry::sequence`].
//!   Used as the query surface for [`AuditQuery`] / [`AuditLog::get`].
//!   The type exposes no public mutate / delete API; the type
//!   system enforces that an inserted entry cannot be modified or
//!   removed.
//! * [`PersistentAuditLog`] — a SQLCipher-backed wrapper that
//!   mirrors every [`AuditLog::append`] to disk and rehydrates the
//!   in-memory log on open. The page-encryption key is derived
//!   from the per-user master key under HKDF context
//!   `b"sqlcipher:audit:v1"`; per-row payloads are encrypted with
//!   XChaCha20-Poly1305 under a per-store AEAD key
//!   (`audit_entry:v1`). The on-disk schema is itself append-only
//!   — there are no `UPDATE` or `DELETE` statements anywhere in
//!   the persistence layer.

#![deny(missing_docs)]

// STABLE
pub mod entry;
// STABLE
pub mod error;
// UNSTABLE — convenience helpers; signatures may change.
#[doc(hidden)]
pub mod helpers;
// STABLE
pub mod log;
// STABLE
pub mod persist;

// STABLE
pub use entry::{
    Actor, AuditActionType, AuditEntry, AuditEntryBuilder, AuditEntryId, TargetRef, TargetType,
};
// STABLE
pub use error::{AuditError, Result};
// UNSTABLE — convenience helpers; signatures may change.
#[doc(hidden)]
pub use helpers::{
    log_export, log_export_simulated, log_proposal_promoted, log_proposal_rejected,
    log_proposal_submitted,
};
// STABLE
pub use log::{AuditLog, AuditQuery};
// STABLE
pub use persist::PersistentAuditLog;
