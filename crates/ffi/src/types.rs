//! FFI-safe wire types — what crosses the bridge.
//!
//! Every type in this module is **plain data**: owned `String`,
//! `Vec<u8>`, `i64`, `f64`, plain enums (no associated data unless
//! the variant truly needs it). That makes the surface trivial to
//! mirror in Swift / Kotlin / TypeScript without bringing the rich
//! Rust types from the rest of the substrate into the binding.
//!
//! The contract guarantees:
//!
//! 1. Every UUID-shaped id crosses the bridge as a UUID string in
//!    canonical hyphenated form.
//! 2. Every timestamp is encoded as an `i64` Unix epoch in seconds.
//! 3. Every enum is `String`-tagged via serde so platform JSON
//!    decoders see stable case labels (`"Reinforced"`,
//!    `"Critical"`, …).

use serde::{Deserialize, Serialize};

/// UUID-string identifier carried across the FFI boundary.
pub type ScopeIdString = String;

/// Source connector that produced an evidence row.
///
/// Mirrors `connector_framework::ConnectorKind` plus a `Manual`
/// catch-all for sideloaded ingest.
// `serde::Deserialize` derives a visitor that takes the variant
// payload by value; clippy attributes the resulting warning to the
// enum declaration. The lint is a false positive against
// derive-generated code (we cannot rewrite the macro output), so the
// allow lives on the type itself rather than the module.
#[allow(clippy::needless_pass_by_value)]
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum SourceKind {
    /// Manually sideloaded by the user.
    Manual,
    /// Slack connector.
    Slack,
    /// Email connector (Gmail or Microsoft Graph).
    Email,
    /// Microsoft Graph connector (Outlook / OneDrive / SharePoint /
    /// Teams).
    MicrosoftGraph,
    /// Atlassian Jira / Confluence connector.
    Atlassian,
    /// HubSpot connector.
    HubSpot,
    /// Google Workspace (Drive, Docs, Calendar) connector.
    GoogleWorkspace,
    /// Other / not yet enumerated.
    Other,
}

/// Importance classification for ingested evidence.
///
/// Mirrors [`evidence_store::ImportanceClass`] as a wire-flat enum.
/// `Critical` and `Important` rows live in the primary evidence
/// table; `Useful` rows may be offloaded sooner; `Noise` rows go
/// directly to the ring buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum FfiImportanceClass {
    /// Must never be evicted (regulatory, compliance).
    Critical,
    /// Default tier — long-lived evidence.
    Important,
    /// Kept but deprioritised for synthesis and retrieval.
    Useful,
    /// Ephemeral; routed to the capped ring buffer.
    Noise,
}

/// One row materialised from the encrypted evidence plane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceRecord {
    /// UUID-string evidence id.
    pub id: String,
    /// UUID-string scope id.
    pub scope_id: ScopeIdString,
    /// Plaintext body (already AEAD-decrypted before crossing the
    /// bridge).
    pub body: String,
    /// Source connector kind.
    pub source: SourceKind,
    /// Unix epoch (seconds) when the row was ingested.
    pub created_at: i64,
}

/// One hit returned by [`super::query`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueryResult {
    /// UUID-string evidence id.
    pub evidence_id: String,
    /// Combined hybrid score in `[0.0, 1.0]`.
    pub score: f64,
    /// FTS contribution component.
    pub fts_score: f64,
    /// Recency contribution component.
    pub recency_score: f64,
    /// Semantic-vector contribution component.
    pub vector_score: f64,
    /// Optional snippet (UI helper — may be empty).
    pub snippet: String,
}

/// Decay state of a memory record. Mirrors
/// `memory_manager::DecayState` but as a wire-flat enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum MemoryState {
    /// Newly observed; awaiting reinforcement to promote.
    Candidate,
    /// Confirmed by reuse; lives at full retention score.
    Reinforced,
    /// Has begun ageing toward archival.
    Decaying,
    /// Cold-archived (encrypted at rest, not in the working set).
    Archived,
    /// Pinned by user — decay-immune.
    Pinned,
}

/// One per-user memory bundle row (a "thing the system remembers
/// about you in this scope").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryRecord {
    /// UUID-string memory id.
    pub id: String,
    /// UUID-string scope id.
    pub scope_id: ScopeIdString,
    /// Human-readable summary of the memory.
    pub summary: String,
    /// Current decay state.
    pub state: MemoryState,
    /// Retention score in `[0.0, 1.0]`.
    pub retention_score: f64,
    /// Unix epoch (seconds) — when this row was first created.
    pub created_at: i64,
    /// Unix epoch (seconds) — last time the row was reinforced.
    pub last_reinforced_at: i64,
}

/// Filter for [`super::list_memories`].
// Same derive-generated false positive as `SourceKind` above.
#[allow(clippy::needless_pass_by_value)]
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryFilter {
    /// If `Some`, restrict to rows in this state.
    pub state: Option<MemoryState>,
    /// If `true`, restrict to rows currently pinned.
    pub pinned_only: bool,
}

/// Reason a synthesis cycle was triggered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum SynthesisTrigger {
    /// User clicked "Synthesise now".
    ManualUserAction,
    /// Idle / background sweep fired by the scheduler.
    BackgroundIdle,
    /// Threshold of unprocessed evidence rows reached.
    EvidenceThreshold,
    /// Connector finished an incremental sync.
    ConnectorSyncCompleted,
}

/// FFI-safe public-key bundle returned by [`super::generate_keypair`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FfiKeypair {
    /// Algorithm tag (`"ml-dsa-65"`, `"sphincs-plus-shake-128f-simple"`).
    pub algorithm: String,
    /// Encoded public verifying key (algorithm-specific bytes).
    pub public_key: Vec<u8>,
    /// Encoded private signing key (algorithm-specific bytes).
    /// Hosts MUST treat this as sensitive.
    pub private_key: Vec<u8>,
}

/// FFI-safe signature blob.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FfiSignature {
    /// Algorithm tag (matches `FfiKeypair::algorithm`).
    pub algorithm: String,
    /// Signature bytes.
    pub bytes: Vec<u8>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_kind_round_trips_via_serde() {
        let kinds = [
            SourceKind::Manual,
            SourceKind::Slack,
            SourceKind::Email,
            SourceKind::MicrosoftGraph,
            SourceKind::Atlassian,
            SourceKind::HubSpot,
            SourceKind::GoogleWorkspace,
            SourceKind::Other,
        ];
        for k in kinds {
            let s = serde_json::to_string(&k).unwrap();
            let back: SourceKind = serde_json::from_str(&s).unwrap();
            assert_eq!(k, back);
        }
    }

    #[test]
    fn memory_state_round_trips_via_serde() {
        let states = [
            MemoryState::Candidate,
            MemoryState::Reinforced,
            MemoryState::Decaying,
            MemoryState::Archived,
            MemoryState::Pinned,
        ];
        for s in states {
            let json = serde_json::to_string(&s).unwrap();
            let back: MemoryState = serde_json::from_str(&json).unwrap();
            assert_eq!(s, back);
        }
    }

    #[test]
    fn synthesis_trigger_round_trips_via_serde() {
        let triggers = [
            SynthesisTrigger::ManualUserAction,
            SynthesisTrigger::BackgroundIdle,
            SynthesisTrigger::EvidenceThreshold,
            SynthesisTrigger::ConnectorSyncCompleted,
        ];
        for t in triggers {
            let json = serde_json::to_string(&t).unwrap();
            let back: SynthesisTrigger = serde_json::from_str(&json).unwrap();
            assert_eq!(t, back);
        }
    }

    #[test]
    fn evidence_record_round_trips_via_serde() {
        let r = EvidenceRecord {
            id: "00000000-0000-0000-0000-000000000001".into(),
            scope_id: "00000000-0000-0000-0000-000000000002".into(),
            body: "hello world".into(),
            source: SourceKind::Slack,
            created_at: 1_700_000_000,
        };
        let s = serde_json::to_string(&r).unwrap();
        let back: EvidenceRecord = serde_json::from_str(&s).unwrap();
        assert_eq!(r, back);
    }

    #[test]
    fn query_result_round_trips_via_serde() {
        let r = QueryResult {
            evidence_id: "00000000-0000-0000-0000-000000000001".into(),
            score: 0.42,
            fts_score: 0.5,
            recency_score: 0.3,
            vector_score: 0.6,
            snippet: "match".into(),
        };
        let s = serde_json::to_string(&r).unwrap();
        let back: QueryResult = serde_json::from_str(&s).unwrap();
        assert_eq!(r, back);
    }

    #[test]
    fn memory_record_round_trips_via_serde() {
        let r = MemoryRecord {
            id: "00000000-0000-0000-0000-000000000001".into(),
            scope_id: "00000000-0000-0000-0000-000000000002".into(),
            summary: "user prefers Lisbon time-zone".into(),
            state: MemoryState::Reinforced,
            retention_score: 0.87,
            created_at: 1_700_000_000,
            last_reinforced_at: 1_700_000_500,
        };
        let s = serde_json::to_string(&r).unwrap();
        let back: MemoryRecord = serde_json::from_str(&s).unwrap();
        assert_eq!(r, back);
    }

    #[test]
    fn memory_filter_default_is_unfiltered() {
        let f = MemoryFilter::default();
        assert!(f.state.is_none());
        assert!(!f.pinned_only);
    }

    #[test]
    fn memory_filter_round_trips_via_serde() {
        let f = MemoryFilter {
            state: Some(MemoryState::Pinned),
            pinned_only: true,
        };
        let s = serde_json::to_string(&f).unwrap();
        let back: MemoryFilter = serde_json::from_str(&s).unwrap();
        assert_eq!(f, back);
    }

    #[test]
    fn ffi_keypair_round_trips_via_serde() {
        let k = FfiKeypair {
            algorithm: "ml-dsa-65".into(),
            public_key: vec![0x01, 0x02, 0x03],
            private_key: vec![0xff, 0xee, 0xdd],
        };
        let s = serde_json::to_string(&k).unwrap();
        let back: FfiKeypair = serde_json::from_str(&s).unwrap();
        assert_eq!(k, back);
    }

    #[test]
    fn ffi_signature_round_trips_via_serde() {
        let s = FfiSignature {
            algorithm: "ml-dsa-65".into(),
            bytes: vec![1, 2, 3, 4],
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: FfiSignature = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }
}
