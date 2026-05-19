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
//! Persistence is intentionally deferred — the current implementation
//! is an in-memory [`TupleStore`] suitable for unit / e2e tests and
//! for server-skeleton work. The on-disk variant is not yet
//! implemented.
//!
//! Cross-references:
//!
//! * Module map: `ARCHITECTURE.md` §2.1.
//! * Permission model: `docs/DESIGN.md` §7.1.

#![deny(missing_docs)]

pub mod check;
pub mod error;
pub mod namespace;
pub mod store;
pub mod tuple;

pub use check::{check_permission, PermissionCheck};
pub use error::{PermissionError, Result};
pub use namespace::{NamespaceConfig, NamespaceRegistry};
pub use store::TupleStore;
pub use tuple::{ObjectRef, ObjectType, Relation, RelationTuple, SubjectRef, SubjectType};
