//! [`AuditEntry`] data model and the builder for safe construction.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use uuid::Uuid;

use evidence_store::ScopeId;

use crate::error::{AuditError, Result};

/// Newtype wrapper for audit entry ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AuditEntryId(pub Uuid);

impl AuditEntryId {
    /// Generate a fresh random audit entry id.
    pub fn new_v4() -> Self {
        Self(Uuid::new_v4())
    }
}

/// Audit-action types per `ARCHITECTURE.md` §4.1 + Phase 3 lifecycle
/// events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditActionType {
    /// A memory object was promoted to canonical.
    CanonicalPromotion,
    /// Data was exported under an export profile.
    Export,
    /// An agent proposed a synthesis or memory change.
    AgentProposal,
    /// A canonical policy was changed.
    PolicyChange,
    /// A tenant member was provisioned.
    MemberProvisioned,
    /// A tenant member was removed.
    MemberRemoved,
    /// A tenant lifecycle transition occurred (Active / Suspended /
    /// Deleted).
    TenantLifecycle,
    /// A tenant root key was destroyed (cryptographic forgetting).
    KeyDestruction,
}

impl AuditActionType {
    /// Stable string tag.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CanonicalPromotion => "canonical_promotion",
            Self::Export => "export",
            Self::AgentProposal => "agent_proposal",
            Self::PolicyChange => "policy_change",
            Self::MemberProvisioned => "member_provisioned",
            Self::MemberRemoved => "member_removed",
            Self::TenantLifecycle => "tenant_lifecycle",
            Self::KeyDestruction => "key_destruction",
        }
    }
}

/// Target object types for audit entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetType {
    /// A tenant.
    Tenant,
    /// A domain.
    Domain,
    /// A channel.
    Channel,
    /// A user.
    User,
    /// A memory object.
    MemoryObject,
    /// A concept-graph node.
    Concept,
    /// A synthesis object / summary.
    Summary,
    /// An export profile.
    ExportProfile,
    /// A canonical policy.
    Policy,
    /// An encryption key.
    Key,
    /// An agent identity.
    Agent,
}

impl TargetType {
    /// Stable string tag.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Tenant => "tenant",
            Self::Domain => "domain",
            Self::Channel => "channel",
            Self::User => "user",
            Self::MemoryObject => "memory_object",
            Self::Concept => "concept",
            Self::Summary => "summary",
            Self::ExportProfile => "export_profile",
            Self::Policy => "policy",
            Self::Key => "key",
            Self::Agent => "agent",
        }
    }
}

/// Reference to the audit target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TargetRef {
    /// Type of the target.
    pub target_type: TargetType,
    /// Id of the target.
    pub target_id: Uuid,
}

impl TargetRef {
    /// Construct a fresh target ref.
    pub fn new(target_type: TargetType, target_id: Uuid) -> Self {
        Self {
            target_type,
            target_id,
        }
    }
}

/// Who took the action.
///
/// `User` = a human; `Agent` = an automated synthesizer / connector;
/// `System` = the substrate itself (e.g. scheduled key destruction).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "id")]
pub enum Actor {
    /// A human user.
    User(Uuid),
    /// An automated agent.
    Agent(Uuid),
    /// The substrate itself.
    System,
}

/// One audit-log entry. Once inserted into an [`crate::AuditLog`],
/// the entry is immutable; the log holds entries by value and never
/// surfaces a `&mut` reference.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuditEntry {
    /// Unique id (UUID v4).
    pub id: AuditEntryId,
    /// Append-order sequence number assigned by the log on insert.
    /// Strictly monotonic per log instance. `0` until the entry is
    /// appended.
    pub sequence: u64,
    /// Wall-clock timestamp.
    pub timestamp: DateTime<Utc>,
    /// Who took the action.
    pub actor: Actor,
    /// Action type.
    pub action_type: AuditActionType,
    /// Target object.
    pub target: TargetRef,
    /// Optional substrate scope (e.g. tenant scope id).
    pub scope_id: Option<ScopeId>,
    /// Free-form action-specific JSON payload.
    pub details: JsonValue,
}

/// Builder for [`AuditEntry`]. The `actor`, `action_type`, and
/// `target` fields are required; the rest default to sensible values.
#[derive(Debug, Default, Clone)]
pub struct AuditEntryBuilder {
    actor: Option<Actor>,
    action_type: Option<AuditActionType>,
    target: Option<TargetRef>,
    scope_id: Option<ScopeId>,
    details: Option<JsonValue>,
    timestamp: Option<DateTime<Utc>>,
}

impl AuditEntryBuilder {
    /// Construct a fresh empty builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the actor.
    pub fn actor(mut self, actor: Actor) -> Self {
        self.actor = Some(actor);
        self
    }

    /// Set the action type.
    pub fn action(mut self, action_type: AuditActionType) -> Self {
        self.action_type = Some(action_type);
        self
    }

    /// Set the target.
    pub fn target(mut self, target: TargetRef) -> Self {
        self.target = Some(target);
        self
    }

    /// Set the scope id.
    pub fn scope(mut self, scope_id: ScopeId) -> Self {
        self.scope_id = Some(scope_id);
        self
    }

    /// Set the JSON details payload.
    pub fn details(mut self, details: JsonValue) -> Self {
        self.details = Some(details);
        self
    }

    /// Override the timestamp (defaults to `Utc::now()`).
    pub fn timestamp(mut self, ts: DateTime<Utc>) -> Self {
        self.timestamp = Some(ts);
        self
    }

    /// Build the entry. The `sequence` starts at `0`; the
    /// [`crate::AuditLog::append`] call assigns the real sequence
    /// number.
    pub fn build(self) -> Result<AuditEntry> {
        let actor = self.actor.ok_or(AuditError::MissingField("actor"))?;
        let action_type = self
            .action_type
            .ok_or(AuditError::MissingField("action_type"))?;
        let target = self.target.ok_or(AuditError::MissingField("target"))?;
        Ok(AuditEntry {
            id: AuditEntryId::new_v4(),
            sequence: 0,
            timestamp: self.timestamp.unwrap_or_else(Utc::now),
            actor,
            action_type,
            target,
            scope_id: self.scope_id,
            details: self.details.unwrap_or(JsonValue::Null),
        })
    }
}
