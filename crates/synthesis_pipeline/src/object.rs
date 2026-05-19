//! [`SynthesisObject`] — typed encrypted output of one synthesis run.
//!
//! Per `docs/DESIGN.md` §6.4: "the synthesis output is published as an
//! encrypted synthesis object back into the scope; other members
//! consume it instead of re-synthesizing".

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use evidence_store::ScopeId;

use crate::window::WindowId;

/// Identifier for a [`SynthesisObject`] (UUID v4 newtype).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ObjectId(pub Uuid);

impl ObjectId {
    /// Generate a fresh random object id.
    pub fn new_v4() -> Self {
        Self(Uuid::new_v4())
    }

    /// Wrap a raw [`Uuid`].
    pub fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid)
    }

    /// Borrow the underlying [`Uuid`].
    pub fn as_uuid(&self) -> Uuid {
        self.0
    }
}

impl std::fmt::Display for ObjectId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Typed payload kind. The four kinds map directly onto the
/// hierarchy from `docs/DESIGN.md` §6 (User → Channel → Domain →
/// Tenant).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SynthesisObjectType {
    /// Personal episodic / session summary.
    EpisodicSummary,
    /// Channel recap. Default — most synthesis runs are channel
    /// recaps.
    #[default]
    ChannelRecap,
    /// Domain-level summary.
    DomainSummary,
    /// Tenant-level institutional summary.
    TenantSummary,
}

impl SynthesisObjectType {
    /// Stable string tag.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::EpisodicSummary => "episodic_summary",
            Self::ChannelRecap => "channel_recap",
            Self::DomainSummary => "domain_summary",
            Self::TenantSummary => "tenant_summary",
        }
    }
}

/// One synthesis object — typed payload, scope-bound, with a pointer
/// to the synthesis window it covers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SynthesisObject {
    /// Unique id (UUID v4).
    pub id: ObjectId,
    /// Scope this object was published into.
    pub scope_id: ScopeId,
    /// Window this object was synthesised over.
    pub window_id: WindowId,
    /// Typed payload kind.
    pub object_type: SynthesisObjectType,
    /// Encrypted (or plaintext, in tests) payload bytes. The
    /// `publish_synthesis_object` round-trip in
    /// [`crate::publish`] wraps these bytes in an
    /// [`crate::publish::EncryptedSynthesisObject`] before
    /// transmission.
    pub payload: Vec<u8>,
    /// Reference to the provenance bundle this object was published
    /// with (typically a `crypto::ProvenanceBundle.entity_id`).
    pub provenance_ref: Uuid,
    /// Wall-clock creation time.
    pub created_at: DateTime<Utc>,
    /// If this object supersedes an earlier object, the earlier
    /// object's id (per `docs/DESIGN.md` §4 — "supersession preferred
    /// over deletion"). The CRDT layer turns the absence of a
    /// supersedes pointer into an add; the presence into a
    /// supersession marker.
    pub supersedes: Option<ObjectId>,
}

impl SynthesisObject {
    /// Construct a fresh synthesis object.
    pub fn new(
        scope_id: ScopeId,
        window_id: WindowId,
        object_type: SynthesisObjectType,
        payload: Vec<u8>,
        provenance_ref: Uuid,
    ) -> Self {
        Self {
            id: ObjectId::new_v4(),
            scope_id,
            window_id,
            object_type,
            payload,
            provenance_ref,
            created_at: Utc::now(),
            supersedes: None,
        }
    }

    /// Mark this object as superseding `predecessor`.
    pub fn with_supersedes(mut self, predecessor: ObjectId) -> Self {
        self.supersedes = Some(predecessor);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supersession_pointer_is_recorded() {
        let scope = ScopeId::new_v4();
        let window = WindowId::new_v4();
        let prev = SynthesisObject::new(
            scope,
            window,
            SynthesisObjectType::ChannelRecap,
            b"old".to_vec(),
            Uuid::nil(),
        );
        let next = SynthesisObject::new(
            scope,
            window,
            SynthesisObjectType::ChannelRecap,
            b"new".to_vec(),
            Uuid::nil(),
        )
        .with_supersedes(prev.id);
        assert_eq!(next.supersedes, Some(prev.id));
    }
}
