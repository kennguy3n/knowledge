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
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, uniffi::Enum)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, uniffi::Enum)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, uniffi::Record)]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, uniffi::Record)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, uniffi::Enum)]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, uniffi::Record)]
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
///
/// `deny_unknown_fields` is applied here (rather than only at the
/// N-API binding layer) because the same `MemoryFilter` shape is
/// serialised across UniFFI (Swift / Kotlin) and the JS bridge.
/// Rejecting unknown keys uniformly at the substrate layer means
/// a typo like `pinnedOnly` (camelCase) or `Pinned_Only`
/// (PascalCase) fails fast with a clear `InvalidArgument` at the
/// FFI boundary on every host, rather than silently producing an
/// empty `state` filter (since `state` is `Option<…>` and would
/// otherwise default to `None`) and then returning a confusing
/// over-broad row set.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize, uniffi::Record)]
#[serde(deny_unknown_fields)]
pub struct MemoryFilter {
    /// If `Some`, restrict to rows in this state.
    pub state: Option<MemoryState>,
    /// If `true`, restrict to rows currently pinned.
    pub pinned_only: bool,
}

/// Reason a synthesis cycle was triggered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, uniffi::Enum)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, uniffi::Record)]
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
///
/// **No `uniffi::Record` derive yet — by design.** UniFFI bindgen
/// only emits Swift / Kotlin types reachable from `#[uniffi::export]`
/// functions; deriving `Record` on a type that no exported function
/// consumes registers metadata that the bindgen quietly drops, which
/// is the kind of dead contract Devin Review flagged on PR #52
/// (`crates/ffi/src/types.rs:186`). The derive is intentionally
/// deferred until a `sign(handle, data) -> FfiResult<FfiSignature>` /
/// `verify(handle, sig, data) -> FfiResult<bool>` FFI pair lands;
/// that PR adds `#[uniffi::export]` on the new functions and the
/// `uniffi::Record` derive on this type in the same commit so the
/// metadata and the consuming export are always in lockstep. The
/// `Serialize` / `Deserialize` derives stay because the
/// `ffi_signature_round_trips_via_serde` unit test and the
/// integration test in `tests/ffi_integration_tests.rs` pin the wire
/// shape so future sign/verify work cannot accidentally rename or
/// reorder the fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FfiSignature {
    /// Algorithm tag (matches `FfiKeypair::algorithm`).
    pub algorithm: String,
    /// Signature bytes.
    pub bytes: Vec<u8>,
}

// ──────────────────────────── Connector wire types ────────────────────────────
//
// Mirrors `connector_framework::ConnectorKind` / `SyncMode` / `SyncStatus`
// as wire-flat UniFFI enums so the substrate's connector-management
// FFI surface (see `super::create_connector`,
// `super::authenticate_connector`, `super::sync_connector`,
// `super::list_connectors`, `super::remove_connector`) can speak the
// same vocabulary as the rest of the codebase without forcing
// mobile / desktop hosts to import the rich types from
// `connector_framework`.
//
// Every type here is `uniffi::Enum` / `uniffi::Record` so it
// crosses the bridge cleanly on Swift / Kotlin / N-API. Strings
// are used for ids (UUIDs in canonical hyphenated form) and for
// the OAuth2 redirect URI / authorisation code arguments.

/// Wire-flat mirror of [`connector_framework::ConnectorKind`].
///
/// Kept in sync with the upstream enum — see
/// `crates/connector_framework/src/config.rs`. Each variant maps
/// to exactly one source-system connector implementation in the
/// `connectors` crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, uniffi::Enum)]
#[serde(rename_all = "snake_case")]
pub enum ConnectorKindTag {
    /// Google Drive — Google Workspace files.
    GoogleDrive,
    /// Microsoft OneDrive — personal + business document libraries.
    OneDrive,
    /// Notion — pages + databases.
    Notion,
    /// Atlassian Jira — issues + projects.
    Jira,
    /// Atlassian Confluence — wiki spaces + pages.
    Confluence,
    /// GitHub — repos + issues + PRs.
    GitHub,
    /// Slack — channels + messages + threads.
    Slack,
    /// Figma — files + frames.
    Figma,
    /// HubSpot — contacts + companies + deals.
    HubSpot,
    /// Email — Gmail or Microsoft Graph mailboxes.
    Email,
    /// Generic webhook receiver — opaque-payload connector for
    /// providers that aren't first-class supported.
    GenericWebhook,
}

/// Sync direction — full re-walk vs. cursor-based incremental.
///
/// Mirrors [`connector_framework::SyncMode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, uniffi::Enum)]
#[serde(rename_all = "snake_case")]
pub enum SyncModeKind {
    /// First-time pull. Walks the entire source surface and
    /// returns the cursor to start incremental sync from.
    Full,
    /// Steady-state pull. Resumes from the last cursor stored in
    /// `SyncState`.
    Incremental,
}

/// Lifecycle phase of a connector's sync state.
///
/// Mirrors [`connector_framework::SyncStatus`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, uniffi::Enum)]
#[serde(rename_all = "snake_case")]
pub enum SyncStatusKind {
    /// Connector exists but no sync has ever run.
    NeverRun,
    /// A sync is currently in flight.
    InProgress,
    /// Most recent sync completed without error.
    Succeeded,
    /// Most recent sync failed; see
    /// [`ConnectorStatus::last_error`] for the diagnostic.
    Failed,
}

/// Wire-flat status row returned by [`super::list_connectors`].
///
/// Carries enough state for the host to render a connectors list
/// view: kind icon, last-synced timestamp, error banner, current
/// sync mode (so the UI can show "Incremental sync" vs "Full
/// sync" badges).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, uniffi::Record)]
pub struct ConnectorStatus {
    /// UUID-string identifier — the
    /// [`connector_framework::ConnectorInstanceId`] this row refers
    /// to. Stable for the lifetime of the connector instance.
    pub instance_id: String,
    /// Which source-system connector this is.
    pub kind: ConnectorKindTag,
    /// UUID-string scope id the connector is bound to. Mirrors
    /// [`connector_framework::ConnectorConfig::scope_id`].
    pub scope_id: ScopeIdString,
    /// Current sync direction (`Full` until the first successful
    /// sync, `Incremental` thereafter).
    pub sync_mode: SyncModeKind,
    /// Most recent lifecycle phase.
    pub sync_status: SyncStatusKind,
    /// Unix epoch (seconds) when the last sync completed, or
    /// `None` if no sync has finished yet.
    pub last_synced_at: Option<i64>,
    /// Last-error diagnostic if `sync_status == Failed`, else
    /// `None`.
    pub last_error: Option<String>,
}

/// Wire-flat result returned by [`super::refresh_connector_token`].
///
/// Records what happened during an explicit token refresh —
/// the host needs `expires_at` so it can schedule the next
/// proactive refresh (or schedule a re-auth UI prompt for tokens
/// whose `expires_at` is approaching), and `refreshed_at` so the
/// host can correlate the refresh event with whatever workflow
/// triggered it (button click, scheduled job, sync that detected
/// expiry).
///
/// `refreshed == false` is reserved for the auto-refresh path in
/// [`super::sync_connector`] where the runtime *checked* whether
/// a refresh was needed and decided no — the explicit refresh
/// entry point [`super::refresh_connector_token`] forces a refresh
/// unconditionally and therefore always returns `refreshed: true`
/// on the success path. Keeping the flag in the wire envelope means
/// hosts that observe a `RefreshReport` in a callback / event log
/// can disambiguate "we did real network work" from "we skipped".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, uniffi::Record)]
pub struct RefreshReport {
    /// UUID-string identifier of the connector instance whose token
    /// was refreshed.
    pub instance_id: String,
    /// `true` if the token was actually refreshed (a network round
    /// trip happened); `false` if the runtime decided the current
    /// token was still fresh enough and short-circuited. The
    /// explicit [`super::refresh_connector_token`] entry point
    /// always returns `true` on success.
    pub refreshed: bool,
    /// Unix epoch seconds when the new access token expires. The
    /// host should use this to schedule the next proactive
    /// refresh (or surface a re-auth prompt if the token has no
    /// `refresh_token`).
    pub expires_at: i64,
    /// Unix epoch seconds when the refresh round-trip completed,
    /// captured AFTER the provider response was processed in the
    /// explicit [`super::refresh_connector_token`] entry point.
    /// Hosts can use this as a correlation / scheduling timestamp
    /// without worrying about it being stale relative to the
    /// network round-trip duration.
    pub refreshed_at: i64,
}

/// Wire-flat result returned by [`super::sync_connector`].
///
/// Records what happened during a single sync run — useful for
/// host-side UI ("Synced 42 new documents from Slack") and for
/// downstream observability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, uniffi::Record)]
pub struct SyncReport {
    /// UUID-string identifier of the connector instance that
    /// produced the run.
    pub instance_id: String,
    /// Which direction this sync ran in.
    pub mode: SyncModeKind,
    /// Total number of [`connector_framework::ConnectorEvent`]
    /// values produced by the run.
    pub events_total: u32,
    /// Subset of `events_total` that were ingested into the
    /// evidence store as new rows (some events — e.g.
    /// `DocumentDeleted` / `PermissionChanged` — do not produce
    /// new evidence rows).
    pub events_ingested: u32,
    /// UUID-string evidence ids freshly created during this sync.
    /// Stable order: emission order of the underlying
    /// `ConnectorEvent` stream.
    pub ingested_evidence_ids: Vec<String>,
    /// Opaque cursor the connector returned for the next
    /// incremental run, or `None` if this connector has no
    /// cursor (e.g. webhook-only sources).
    pub next_cursor: Option<String>,
    /// Unix epoch (seconds) when the run started.
    pub started_at: i64,
    /// Unix epoch (seconds) when the run completed.
    pub completed_at: i64,
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
    fn refresh_report_round_trips_via_serde() {
        let r = RefreshReport {
            instance_id: "00000000-0000-0000-0000-000000000001".into(),
            refreshed: true,
            expires_at: 1_900_000_000,
            refreshed_at: 1_800_000_000,
        };
        let s = serde_json::to_string(&r).unwrap();
        let back: RefreshReport = serde_json::from_str(&s).unwrap();
        assert_eq!(r, back);
        // Also pin the `refreshed: false` discriminator the
        // sync_connector auto-refresh path emits when the token
        // is still fresh.
        let skipped = RefreshReport {
            instance_id: "00000000-0000-0000-0000-000000000002".into(),
            refreshed: false,
            expires_at: 1_900_000_000,
            refreshed_at: 1_800_000_000,
        };
        let s = serde_json::to_string(&skipped).unwrap();
        let back: RefreshReport = serde_json::from_str(&s).unwrap();
        assert_eq!(skipped, back);
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
    fn memory_filter_rejects_camelcase_pinned_only_alias() {
        // Pins the `deny_unknown_fields` invariant — a JS / Swift
        // caller using `pinnedOnly` (camelCase) instead of the
        // canonical `pinned_only` (snake_case) must surface as a
        // clear deserialization error, not silently default to
        // `pinned_only = false`. This protects every host that
        // marshals into `MemoryFilter` (N-API, UniFFI Swift,
        // UniFFI Kotlin) at the substrate level.
        let payload = r#"{"state":"Pinned","pinnedOnly":true,"pinned_only":true}"#;
        let err = serde_json::from_str::<MemoryFilter>(payload)
            .expect_err("MemoryFilter must reject unknown camelCase keys like `pinnedOnly`");
        let msg = err.to_string();
        assert!(
            msg.contains("unknown field") && msg.contains("pinnedOnly"),
            "expected `unknown field `pinnedOnly``, got {msg}"
        );
    }

    #[test]
    fn memory_filter_rejects_stray_unknown_field() {
        // Anything other than `state` / `pinned_only` must error.
        let payload = r#"{"state":null,"pinned_only":false,"junk":42}"#;
        let err = serde_json::from_str::<MemoryFilter>(payload)
            .expect_err("MemoryFilter must reject stray unknown keys");
        assert!(
            err.to_string().contains("unknown field"),
            "expected `unknown field` error, got {err}"
        );
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
