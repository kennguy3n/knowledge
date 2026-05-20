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
//! The crate exposes two layered registries:
//!
//! * [`TenantRegistry`] — an in-memory map keyed by [`TenantId`]
//!   used as the query surface by the synthesis engine, permission
//!   service, and audit log.
//! * [`PersistentTenantRegistry`] — a SQLCipher-backed wrapper that
//!   mirrors every mutation to disk and rehydrates the in-memory
//!   registry on open. The page-encryption key is derived from
//!   the per-user master key under HKDF context
//!   `b"sqlcipher:tenants:v1"`; per-tenant payloads are encrypted
//!   with XChaCha20-Poly1305 under a per-store AEAD key
//!   (`tenant_row:v1`). Member rows and configs are stored in
//!   plaintext because the substrate's threat model already
//!   exposes that taxonomy via the permission graph (see
//!   `docs/DESIGN.md` §7.1).

#![deny(missing_docs)]

pub mod config;
pub mod error;
pub mod lifecycle;
pub mod member;
pub mod persist;
pub mod tenant;

pub use config::{StorageConfig, SynthesisConfig, TenantConfig, TenantKeyRef};
pub use error::{Result, TenantError};
pub use lifecycle::TenantStatus;
pub use member::{TenantMember, TenantMemberStatus};
pub use persist::PersistentTenantRegistry;
pub use tenant::{Tenant, TenantId, TenantRegistry};
