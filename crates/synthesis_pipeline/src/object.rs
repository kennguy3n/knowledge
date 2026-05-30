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
    /// Monotonically increasing version stamp for the window. The
    /// original dispatch lands at `version = 1`; each
    /// `replay_synthesis` call on the same `(scope_id, window_id)`
    /// pair bumps the stamp by 1 and stores the previous object in
    /// the `synthesis_object_versions` history table before
    /// installing itself as the new "latest" version in
    /// `synthesis_objects`.
    ///
    /// `#[serde(default = "default_synthesis_object_version")]` so
    /// blobs persisted before this field was introduced rehydrate
    /// cleanly as `version: 1`, matching the implicit pre-versioning
    /// contract.
    #[serde(default = "default_synthesis_object_version")]
    pub version: u32,
}

/// Initial / serde-fallback version stamp for `SynthesisObject`.
///
/// Exposed as a free function (rather than `Default::default`) so
/// the `#[serde(default = ...)]` derive on the `version` field can
/// reference it directly, and so callers can spell out the intent
/// "this is the first version" without sprinkling the magic literal
/// `1u32` across the codebase.
pub const fn default_synthesis_object_version() -> u32 {
    1
}

impl SynthesisObject {
    /// Construct a fresh synthesis object with the initial version
    /// stamp (`version = 1`).
    pub fn new(
        scope_id: ScopeId,
        window_id: WindowId,
        object_type: SynthesisObjectType,
        payload: Vec<u8>,
        provenance_ref: Uuid,
    ) -> Self {
        Self::with_version(
            scope_id,
            window_id,
            object_type,
            payload,
            provenance_ref,
            default_synthesis_object_version(),
        )
    }

    /// Construct a synthesis object at an explicit version stamp.
    /// Used by `replay_synthesis` (and its tests) to mint a fresh
    /// `ObjectId` while carrying a non-default version. New objects
    /// outside the replay path should call [`Self::new`] instead so
    /// the version-stamp invariant stays implicit in the type.
    pub fn with_version(
        scope_id: ScopeId,
        window_id: WindowId,
        object_type: SynthesisObjectType,
        payload: Vec<u8>,
        provenance_ref: Uuid,
        version: u32,
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
            version,
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
