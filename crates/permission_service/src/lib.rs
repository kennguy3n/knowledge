//! `permission_service` — Zanzibar-style permission service for the
//! Knowledge substrate.
//!
//! Per `ARCHITECTURE.md` §6 and `docs/DESIGN.md` §7.1, every access
//! decision in the substrate is a reachability query over a graph of
//! **relation tuples**:
//!
//! ```text
//! (object_type, object_id) # relation @ (subject_type, subject_id)
//! ```
//!
//! For example:
//!
//! * `(Tenant, t-1) # owner @ (User, u-42)`
//! * `(Domain, d-9) # editor @ (Tenant, t-1) # admin`
//! * `(Channel, c-3) # viewer @ (User, u-7)`
//!
//! `check(object, relation, subject)` walks the relation graph,
//! folding in **namespace inheritance** (e.g. `owner` ⇒ `admin` ⇒
//! `editor` ⇒ `member` ⇒ `viewer`), to decide whether the subject can
//! exercise the relation on the object. The walk follows
//! [`RelationTuple::subject_relation`] pointers (the *userset
//! rewrite* leg of the Zanzibar model), so a tuple of the form
//! `(D, d-9) # editor @ (Tenant, t-1) # admin` resolves by recursing
//! into `(Tenant, t-1) # admin @ ?`.
//!
//! The crate exposes two layered stores:
//!
//! * [`TupleStore`] — an in-memory `HashSet`-backed view used as the
//!   query surface by `check_permission` and friends.
//! * [`PersistentTupleStore`] — a SQLCipher-backed wrapper that
//!   mirrors every mutation to disk and rehydrates the in-memory
//!   view on open. The page-encryption key is derived from the
//!   per-user master key under HKDF context
//!   `b"sqlcipher:permissions:v1"`; per-row payloads are encrypted
//!   under a per-store AEAD key (`permission_tuple:v1`).
//!
//! Cross-references:
//!
//! * Module map: `ARCHITECTURE.md` §2.1.
//! * Permission model: `docs/DESIGN.md` §7.1.

#![deny(missing_docs)]

// STABLE
pub mod check;
// STABLE
pub mod error;
// STABLE
pub mod namespace;
// STABLE
pub mod persist;
// STABLE
pub mod store;
// STABLE
pub mod tuple;

// STABLE
pub use check::{check_permission, PermissionCheck};
// STABLE
pub use error::{PermissionError, Result};
// STABLE
pub use namespace::{NamespaceConfig, NamespaceRegistry};
// STABLE
pub use persist::PersistentTupleStore;
// STABLE
pub use store::TupleStore;
// STABLE
pub use tuple::{ObjectRef, ObjectType, Relation, RelationTuple, SubjectRef, SubjectType};
