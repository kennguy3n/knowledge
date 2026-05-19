//! `tenant_service` — tenant lifecycle and member-provisioning data
//! model.
//!
//! Per `ARCHITECTURE.md` §4.1, the tenant service owns:
//!
//! * **Tenant lifecycle.** A [`Tenant`] moves through
//!   `Active → Suspended → Deleted`. Deletion records a reference to
//!   the destroyed tenant root key (cryptographic forgetting) so the
//!   audit log can prove the tenant is unrecoverable.
//! * **Per-tenant configuration.** A [`TenantConfig`] holds the
//!   handles for the tenant's encryption keys, storage configuration,
//!   and synthesis configuration. The service does not itself perform
//!   crypto — it stores opaque [`crypto::ContentHash`]-shaped key
//!   *references* and trusts the `crypto` crate for the heavy lifting.
//! * **Member provisioning.** [`TenantMember`] entries track which
//!   users belong to a tenant, with what [`permission_service::Relation`]
//!   role.
//!
//! The current implementation is in-memory and is wired into the
//! server-side synthesis engine + permission service. Persistence
//! (Postgres for the tenant catalog, key store for the root keys)
//! is not yet implemented.

#![deny(missing_docs)]

pub mod config;
pub mod error;
pub mod lifecycle;
pub mod member;
pub mod tenant;

pub use config::{StorageConfig, SynthesisConfig, TenantConfig, TenantKeyRef};
pub use error::{Result, TenantError};
pub use lifecycle::TenantStatus;
pub use member::{TenantMember, TenantMemberStatus};
pub use tenant::{Tenant, TenantId, TenantRegistry};
