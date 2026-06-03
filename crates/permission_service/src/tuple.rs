//! Relation tuples — the unit of authorisation data.
//!
//! Per `docs/technical/architecture.md` §6, a relation tuple binds a (typed) object,
//! a relation, and a (typed) subject — optionally itself rewritten
//! through a relation:
//!
//! ```text
//! (object_type, object_id) # relation @ (subject_type, subject_id) [# subject_relation]
//! ```

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Object types per `docs/technical/architecture.md` §6 / `docs/technical/design.md` §7.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObjectType {
    /// A tenant — the highest scope in the B2B hierarchy.
    #[default]
    Tenant,
    /// A domain inside a tenant.
    Domain,
    /// A channel inside a domain (or a community in B2C).
    Channel,
    /// A user account.
    User,
    /// A device bound to a user.
    Device,
    /// A concept in the concept graph.
    Concept,
    /// A synthesis summary object.
    Summary,
    /// A workflow definition.
    Workflow,
    /// An export profile (per-relation export controls).
    ExportProfile,
    /// An agent (synthesizer / connector / managed AI endpoint).
    Agent,
}

impl ObjectType {
    /// Stable string tag used for serialisation / debugging.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Tenant => "tenant",
            Self::Domain => "domain",
            Self::Channel => "channel",
            Self::User => "user",
            Self::Device => "device",
            Self::Concept => "concept",
            Self::Summary => "summary",
            Self::Workflow => "workflow",
            Self::ExportProfile => "export_profile",
            Self::Agent => "agent",
        }
    }
}

/// Subject side of a relation tuple — currently the same kinds as
/// objects, plus the convention that an `Agent` may stand in for the
/// substrate's automated synthesizers.
pub type SubjectType = ObjectType;

/// The relations the substrate distinguishes per `docs/technical/architecture.md`
/// §6 / `docs/technical/design.md` §7.1. The default inheritance chain (see
/// [`crate::namespace`]) is:
///
/// `Owner ⇒ Admin ⇒ Editor ⇒ Member ⇒ Viewer`
///
/// `Synthesizer` and `Proposer` are orthogonal ambient roles that do
/// not imply membership; they are checked directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Relation {
    /// Tenant / domain / channel owner.
    Owner,
    /// Administrator — full management rights.
    Admin,
    /// May edit content within scope.
    Editor,
    /// Regular member — may view and contribute messages.
    Member,
    /// Read-only.
    Viewer,
    /// May act as the elected synthesizer for a scope.
    Synthesizer,
    /// May propose new canonical concepts / decisions.
    Proposer,
}

impl Relation {
    /// Stable string tag used for serialisation / debugging.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Admin => "admin",
            Self::Editor => "editor",
            Self::Member => "member",
            Self::Viewer => "viewer",
            Self::Synthesizer => "synthesizer",
            Self::Proposer => "proposer",
        }
    }
}

/// A typed object reference: `(type, id)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ObjectRef {
    /// Type of the object.
    pub object_type: ObjectType,
    /// Id of the object.
    pub object_id: Uuid,
}

impl ObjectRef {
    /// Construct a new object ref.
    pub fn new(object_type: ObjectType, object_id: Uuid) -> Self {
        Self {
            object_type,
            object_id,
        }
    }
}

/// A typed subject reference: `(type, id)` with an optional
/// `subject_relation` tag for userset rewrites.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SubjectRef {
    /// Type of the subject.
    pub subject_type: SubjectType,
    /// Id of the subject.
    pub subject_id: Uuid,
    /// Optional rewrite — `Some(r)` means the subject is *the set of
    /// principals related to `(subject_type, subject_id)` via `r`*,
    /// i.e. the Zanzibar `#` indirection.
    pub subject_relation: Option<Relation>,
}

impl SubjectRef {
    /// A direct subject (no userset rewrite).
    pub fn direct(subject_type: SubjectType, subject_id: Uuid) -> Self {
        Self {
            subject_type,
            subject_id,
            subject_relation: None,
        }
    }

    /// A subject *via* a relation: `(subject_type, subject_id) # rel`.
    pub fn via(subject_type: SubjectType, subject_id: Uuid, relation: Relation) -> Self {
        Self {
            subject_type,
            subject_id,
            subject_relation: Some(relation),
        }
    }
}

/// One relation tuple.
///
/// A tuple says: *the principal `subject` is in the `relation`-set of
/// `object`*. When `subject.subject_relation` is `Some(r)`, the
/// principal is *anything that is related to `subject` via `r`*.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RelationTuple {
    /// Object side.
    pub object: ObjectRef,
    /// Relation type.
    pub relation: Relation,
    /// Subject side.
    pub subject: SubjectRef,
}

impl RelationTuple {
    /// Construct a new tuple.
    pub fn new(object: ObjectRef, relation: Relation, subject: SubjectRef) -> Self {
        Self {
            object,
            relation,
            subject,
        }
    }
}
