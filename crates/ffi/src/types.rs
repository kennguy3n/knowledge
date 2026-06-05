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
    /// BCP-47 primary language subtag detected on the plaintext
    /// body at ingest time (schema v13). `None` when
    /// the row was ingested via the legacy
    /// `EvidenceStore::ingest()` shim, when the language detector
    /// declined to classify (empty / pure-punctuation / pure-emoji
    /// / unreliable short input), or when the row predates schema
    /// v13.
    ///
    /// Host bindings (Swift / Kotlin / Electron) MUST treat `None`
    /// as *language unknown* rather than substitute a default —
    /// see [`EvidenceStore::ingest_with_language`]. The
    /// `#[serde(default)]` attribute keeps the field
    /// forward-compatible with pre-v13 host bridges that emit
    /// `EvidenceRecord` JSON without the key.
    ///
    /// [`EvidenceStore::ingest_with_language`]:
    ///     ../../evidence_store/struct.EvidenceStore.html#method.ingest_with_language
    #[serde(default)]
    pub language_tag: Option<String>,
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
/// **No `uniffi::Record` derive — by design.** UniFFI bindgen
/// only emits Swift / Kotlin types reachable from `#[uniffi::export]`
/// functions; deriving `Record` on a type that no exported function
/// consumes registers metadata that the bindgen quietly drops, a
/// dead contract. The derive belongs alongside a `sign(handle, data)
/// -> FfiResult<FfiSignature>` / `verify(handle, sig, data) ->
/// FfiResult<bool>` FFI pair: the functions carry `#[uniffi::export]`
/// and this type carries `uniffi::Record` in the same commit, so the
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
    /// Salesforce — CRM cases + records.
    Salesforce,
    /// ServiceNow — ITSM incidents + records.
    ServiceNow,
    /// Zendesk — support tickets.
    Zendesk,
    /// Linear — issues.
    Linear,
    /// Asana — tasks.
    Asana,
    /// Monday.com — board items.
    Monday,
    /// ClickUp — tasks.
    ClickUp,
    /// Freshdesk — support tickets.
    Freshdesk,
    /// Intercom — conversations.
    Intercom,
    /// Pipedrive — deals.
    Pipedrive,
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
///
/// # Wire format
///
/// `Serialize`/`Deserialize` use `rename_all = "camelCase"` so
/// the JSON surface used by the N-API crate stays consistent
/// with the other JSON-returning entry points
/// ([`crate::refresh_connector_token`],
/// [`crate::sync_connector`], [`crate::list_webhook_servers`],
/// [`crate::sync_scheduler_status`]). A JS host destructuring
/// the value can use
/// `{ instanceId, kind, scopeId, syncMode, syncStatus,
///    lastSyncedAt, lastError }` directly. UniFFI bindings
/// (Swift/Kotlin) are unaffected — they read Rust field names
/// directly through the `uniffi::Record` derive and do not
/// pass through `serde`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, uniffi::Record)]
#[serde(rename_all = "camelCase")]
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

/// Wire-flat result returned by [`super::connector_status`]
/// (single-instance health probe symmetric with
/// [`super::synthesis_status`]).
///
/// Bundles three independent slices of per-connector state that
/// previously required three FFI calls + a manual join:
///
/// 1. The base [`ConnectorStatus`] fields (kind, scope, sync mode
///    / status, last-synced timestamp, last error) — same source
///    of truth as the entry in [`super::list_connectors`].
/// 2. Scheduler posture from
///    [`crate::sync_scheduler::sync_scheduler_status`] — whether
///    the dispatch worker is running, the effective
///    `sync_interval` / `max_backoff` for this instance (host
///    override if [`super::configure_sync_schedule`] supplied
///    one, otherwise the scheduler defaults), and the
///    `auto_synthesize` flag.
/// 3. Backoff posture — `consecutive_failures` since the last
///    successful sync, `next_attempt_unix` for the next scheduled
///    dispatch (or `None` if the scheduler is stopped / the
///    instance has never been scheduled), and an `in_cooldown`
///    convenience flag (`true` iff the instance is past its
///    first failure and the scheduler is actively backing off).
///
/// `is_scheduled` separates "scheduler is running and this
/// instance is registered with it" from the legacy
/// `ConnectorStatus` view — a host that calls
/// [`super::create_connector`] but never starts the scheduler
/// will see `is_scheduled=false` even though `last_synced_at`
/// may be populated from host-driven [`super::sync_connector`]
/// calls.
///
/// # Wire format
///
/// `Serialize`/`Deserialize` use `rename_all = "camelCase"` so the
/// JSON surface matches the other connector-shaped records.
/// `{ instanceId, kind, scopeId, syncMode, syncStatus,
///    lastSyncedAt, lastError, isScheduled, syncIntervalSecs,
///    maxBackoffSecs, autoSynthesize, consecutiveFailures,
///    nextAttemptUnix, inCooldown }` is the JS-idiomatic shape.
/// UniFFI bindings (Swift/Kotlin) are unaffected — they read
/// Rust field names directly through the `uniffi::Record` derive
/// and do not pass through `serde`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, uniffi::Record)]
#[serde(rename_all = "camelCase")]
pub struct ConnectorHealthRecord {
    /// UUID-string identifier — the
    /// [`connector_framework::ConnectorInstanceId`] this row refers to.
    pub instance_id: String,
    /// Which source-system connector this is.
    pub kind: ConnectorKindTag,
    /// UUID-string scope id the connector is bound to.
    pub scope_id: ScopeIdString,
    /// Current sync direction.
    pub sync_mode: SyncModeKind,
    /// Most recent lifecycle phase.
    pub sync_status: SyncStatusKind,
    /// Unix epoch seconds when the last sync completed, or `None`
    /// if no sync has finished yet.
    pub last_synced_at: Option<i64>,
    /// Last-error diagnostic if `sync_status == Failed`, else
    /// `None`.
    pub last_error: Option<String>,
    /// `true` iff [`crate::sync_scheduler::start_sync_scheduler`]
    /// is currently running on this runtime. When `true`, the
    /// scheduler will fire this instance on the next tick that
    /// matches its `next_attempt_at` — regardless of whether the
    /// host has supplied a per-instance policy override via
    /// [`crate::sync_scheduler::configure_sync_schedule`] (absent
    /// overrides fall back to the scheduler's `default_interval`
    /// / `default_max_backoff`).
    ///
    /// The per-instance `SchedulePolicy` table that backs
    /// [`crate::sync_scheduler::configure_sync_schedule`] and
    /// [`crate::sync_scheduler::configure_sync_auto_synthesize`]
    /// lives inside the running scheduler value and is dropped
    /// on `stop_sync_scheduler`. Both configuration FFIs require
    /// the scheduler to be running (they return
    /// [`FfiError::Connector`] otherwise). After a
    /// stop/start cycle the per-instance policies are gone —
    /// hosts that need their overrides back must re-apply them
    /// after [`crate::sync_scheduler::start_sync_scheduler`].
    pub is_scheduled: bool,
    /// Effective per-instance sync interval in seconds. Reflects
    /// either the host-supplied
    /// [`crate::sync_scheduler::configure_sync_schedule`]
    /// override, or — absent an override — the scheduler's
    /// `default_interval` configured at
    /// [`crate::sync_scheduler::start_sync_scheduler`] time.
    /// `0` iff the scheduler is not running on this runtime
    /// (no default to report against).
    pub sync_interval_secs: u64,
    /// Effective per-instance max backoff cap in seconds. Same
    /// override-then-default semantics as [`Self::sync_interval_secs`];
    /// `0` iff the scheduler is not running.
    pub max_backoff_secs: u64,
    /// `true` iff the scheduler will fire a domain-tier
    /// [`crate::synthesis::trigger_server_synthesis`] against the
    /// connector's scope after each successful sync. Toggled per
    /// instance by
    /// [`crate::sync_scheduler::configure_sync_auto_synthesize`].
    /// `false` when the scheduler is stopped — the flag lives in
    /// the running scheduler's policy table and does not survive a
    /// `stop_sync_scheduler` / `start_sync_scheduler` cycle. See
    /// [`Self::is_scheduled`] for the full lifetime contract.
    pub auto_synthesize: bool,
    /// Consecutive failures since the last successful sync.
    /// Reset to `0` on success. Used by the scheduler as the
    /// exponent in the next-attempt-delay calculation. Mirrors
    /// the value reported in
    /// [`crate::sync_scheduler::sync_scheduler_status`].
    pub consecutive_failures: u32,
    /// Unix epoch seconds for the next scheduled dispatch
    /// attempt, or `None` if the scheduler is stopped or has
    /// never dispatched this instance (in which case the next
    /// tick fires it immediately).
    pub next_attempt_unix: Option<i64>,
    /// Convenience flag for hosts that want a single
    /// "is this connector currently backing off?" check.
    /// `true` iff [`Self::is_scheduled`] is `true` AND
    /// [`Self::consecutive_failures`] is greater than zero.
    /// A successful-but-pending instance (failures==0,
    /// next_attempt_unix in the future) is NOT in cooldown — the
    /// scheduler is just waiting for the regular `sync_interval`
    /// to elapse.
    pub in_cooldown: bool,
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
///
/// # Wire format
///
/// `Serialize`/`Deserialize` use `rename_all = "camelCase"` so the
/// JSON surface used by the N-API crate matches the JS naming
/// convention documented at [`crate::refresh_connector_token`]
/// (`{ instanceId, refreshed, expiresAt, refreshedAt }`). UniFFI
/// bindings (Swift/Kotlin) are unaffected — they read Rust field
/// names directly through the `uniffi::Record` derive and do not
/// pass through `serde`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, uniffi::Record)]
#[serde(rename_all = "camelCase")]
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
///
/// # Wire format
///
/// `Serialize`/`Deserialize` use `rename_all = "camelCase"` so the
/// JSON surface used by the N-API crate stays consistent with the
/// other JSON-returning entry points
/// ([`crate::refresh_connector_token`],
/// [`crate::list_webhook_servers`], [`crate::sync_scheduler_status`]).
/// A JS host destructuring the value can use
/// `{ instanceId, mode, eventsTotal, eventsIngested,
///    ingestedEvidenceIds, nextCursor, startedAt, completedAt }`
/// directly. UniFFI bindings (Swift/Kotlin) are unaffected —
/// they read Rust field names directly through the
/// `uniffi::Record` derive and do not pass through `serde`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, uniffi::Record)]
#[serde(rename_all = "camelCase")]
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

/// Opaque handle to one running webhook receiver server.
///
/// Allocated by [`super::start_webhook_server`] and re-presented to
/// [`super::stop_webhook_server`] / [`super::register_webhook_dispatch`]
/// / [`super::unregister_webhook_dispatch`] to identify which server
/// the call targets. The underlying type is `u64`; hosts treat it as
/// opaque — every server allocation comes from a monotonically
/// increasing counter so `0` is never minted as a real handle
/// (mirrors [`super::RuntimeHandle::NONE`]'s sentinel discipline).
///
/// Wrapped in a UniFFI newtype so Swift / Kotlin see it as a
/// dedicated type (`WebhookServerHandle` rather than a bare `UInt64`)
/// and can't accidentally swap it with a [`super::RuntimeHandle`] of
/// the same width.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
#[repr(transparent)]
pub struct WebhookServerHandle(pub u64);

uniffi::custom_newtype!(WebhookServerHandle, u64);

impl WebhookServerHandle {
    /// Sentinel "no server" handle. Never minted by
    /// [`super::start_webhook_server`].
    pub const NONE: WebhookServerHandle = WebhookServerHandle(0);
}

/// Wire-flat summary row returned by [`super::list_webhook_servers`].
///
/// Carries enough state for the host to render a server-list view —
/// which port is bound, how many registrations are live, how many
/// successful dispatches it has fanned, how many dispatches failed.
/// All counters reset on `start_webhook_server` and remain monotonic
/// across the server's lifetime.
///
/// # Wire format
///
/// `Serialize`/`Deserialize` use `rename_all = "camelCase"` so the JSON
/// surface used by the N-API crate matches the JS naming convention
/// documented at [`crate::list_webhook_servers`]
/// (`{ serverHandle, bindAddr, startedAt, registrationCount,
///    dispatchOkTotal, dispatchBadRequestTotal,
///    dispatchBadGatewayTotal }`). UniFFI bindings (Swift/Kotlin)
/// are unaffected — they read Rust field names directly through the
/// `uniffi::Record` derive and do not pass through `serde`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, uniffi::Record)]
#[serde(rename_all = "camelCase")]
pub struct WebhookServerSummary {
    /// Stable opaque handle this row refers to. Same value the host
    /// originally received from [`super::start_webhook_server`].
    pub server_handle: WebhookServerHandle,
    /// Bound socket address (`ip:port`) — resolved AFTER the OS
    /// picked the ephemeral port if the caller requested `:0`.
    /// Hosts whose ingress / NAT setup discovers the live port from
    /// this field MUST query [`super::list_webhook_servers`] (the
    /// summary is the only way the caller learns the resolved port).
    pub bind_addr: String,
    /// Unix epoch seconds when [`super::start_webhook_server`]
    /// returned. Useful for the host's audit log + for diagnostics
    /// that correlate webhook-side activity with start-of-day.
    pub started_at: i64,
    /// How many `(provider_id, instance_id)` dispatch rows are
    /// currently registered against this server.
    pub registration_count: u32,
    /// Total dispatches that completed with `200 OK` since the
    /// server started (i.e. the dispatcher returned
    /// `Ok(())` from [`connector_framework::WebhookDispatcher::dispatch`]
    /// and the underlying connector's `handle_webhook_event` produced
    /// at least one event row — or zero rows, which is also success).
    pub dispatch_ok_total: u64,
    /// Total dispatches that completed with `400 Bad Request`
    /// (the dispatcher returned `ConnectorError::Webhook(_)`,
    /// translating malformed-payload errors to a 4xx so the
    /// upstream provider stops re-delivering).
    pub dispatch_bad_request_total: u64,
    /// Total dispatches that completed with `502 Bad Gateway`
    /// (the dispatcher returned any other `ConnectorError`,
    /// translating substrate-side failures to a 5xx so the
    /// upstream provider's retry-with-backoff is invoked).
    pub dispatch_bad_gateway_total: u64,
}

/// Wire-flat diagnostic snapshot returned by
/// [`super::sync_scheduler_status`].
///
/// Reports the background scheduler's running state, configuration
/// echo, and per-counter totals so hosts can render a "Sync
/// scheduler: running (11 instances driven, 3 with custom policy,
/// 12 ticks, 7 dispatches, 6 succeeded, 1 failed)" badge without
/// enumerating connectors independently. All counters are monotonic
/// across the scheduler's lifetime and reset to zero on a fresh
/// [`super::start_sync_scheduler`].
///
/// # Wire format
///
/// `Serialize`/`Deserialize` use `rename_all = "camelCase"` so the JSON
/// surface used by the N-API crate matches the JS naming convention
/// documented at [`crate::sync_scheduler_status`]
/// (`{ isRunning, startedAtUnix, defaultIntervalSecs,
///    defaultMaxBackoffSecs, tickIntervalSecs, policyOverrideCount,
///    totalInstanceCount, lastTickAtUnix, ticksCompleted,
///    dispatchesAttempted, dispatchesSucceeded, dispatchesFailed,
///    dispatchesSkippedInProgress }`). UniFFI bindings (Swift/Kotlin)
/// are unaffected — they read Rust field names directly through the
/// `uniffi::Record` derive and do not pass through `serde`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, uniffi::Record)]
#[serde(rename_all = "camelCase")]
pub struct SyncSchedulerStatus {
    /// `true` iff a scheduler is currently running on this runtime
    /// handle. `false` includes both "never started" and "started
    /// then explicitly stopped"; the host should call
    /// [`super::start_sync_scheduler`] in either case to start
    /// dispatching.
    pub is_running: bool,
    /// Unix epoch seconds when the most recent
    /// [`super::start_sync_scheduler`] call returned. `None` when
    /// `is_running == false`.
    pub started_at_unix: Option<i64>,
    /// Default per-instance sync interval (`0` when not running).
    /// Echo of the `default_interval_secs` argument supplied at
    /// start time.
    pub default_interval_secs: u64,
    /// Default per-instance backoff cap (`0` when not running).
    pub default_max_backoff_secs: u64,
    /// Tick cadence (`0` when not running).
    pub tick_interval_secs: u64,
    /// Number of connector instances with a per-instance policy
    /// override (set via [`super::configure_sync_schedule`]). This
    /// is a strict subset of [`Self::total_instance_count`] — every
    /// instance with an override is also a connector instance the
    /// scheduler considers, but instances without an override are
    /// counted only in `total_instance_count`.
    ///
    /// Was named `scheduled_instance_count` in earlier revisions of
    /// this FFI surface; later renamed to disambiguate from
    /// `total_instance_count`. A host UI that wants "how many
    /// connectors is the scheduler driving" should read
    /// `total_instance_count`; this field reports the strictly
    /// smaller "how many connectors have a custom policy set".
    pub policy_override_count: u32,
    /// Total number of connector instances the scheduler walks on
    /// every tick — i.e. the size of
    /// [`crate::runtime::FfiRuntime::connector_instances`]. Each
    /// such instance is dispatched on the scheduler's default
    /// policy unless it appears in [`Self::policy_override_count`]
    /// (in which case it uses the custom
    /// [`super::configure_sync_schedule`] policy instead).
    ///
    /// `0` when not running (matches the convention used by every
    /// other numeric field on this struct when `is_running == false`).
    pub total_instance_count: u32,
    /// Unix epoch seconds of the scheduler's most recent tick.
    /// `None` until the first tick has fired (a freshly-started
    /// scheduler may show `is_running=true, last_tick_at_unix=None`
    /// for up to `tick_interval_secs` seconds).
    pub last_tick_at_unix: Option<i64>,
    /// Total ticks the worker thread has completed since
    /// `start_sync_scheduler`. Includes ticks that found no due
    /// instances.
    pub ticks_completed: u64,
    /// Total dispatch attempts initiated by the scheduler. Counts
    /// `sync_connector` calls made by the worker, not their
    /// success/failure.
    pub dispatches_attempted: u64,
    /// Total dispatches that completed with `Ok(SyncReport)`.
    pub dispatches_succeeded: u64,
    /// Total dispatches that completed with `Err(_)`. Used as the
    /// exponent in the per-instance backoff curve.
    pub dispatches_failed: u64,
    /// Total ticks where the scheduler decided NOT to dispatch a
    /// candidate instance because it was already in
    /// [`connector_framework::SyncStatus::InProgress`] (a
    /// host-driven sync was running concurrently). Distinct from
    /// `dispatches_failed` because the scheduler never invoked
    /// `sync_connector` for these.
    pub dispatches_skipped_in_progress: u64,
}

/// Tier of server-side synthesis to dispatch.
///
/// Mirrors [`synthesis_pipeline::WindowScopeTier`] for the
/// hierarchy-enforced server-side tiers exposed by
/// [`crate::synthesis::trigger_server_synthesis`]. Channel-tier
/// synthesis is handled by the on-device
/// [`crate::trigger_synthesis`] path and is therefore not part of
/// this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, uniffi::Enum)]
#[serde(rename_all = "snake_case")]
pub enum SynthesisTierKind {
    /// Domain synthesis — consumes channel outputs registered on
    /// the target domain's `channel_scopes` list and emits a
    /// `DomainSummary` synthesis object.
    Domain,
    /// Tenant synthesis — consumes domain outputs and approved
    /// documents registered on the target tenant's
    /// `domain_scopes` / `approved_documents` lists and emits a
    /// `TenantSummary` synthesis object.
    Tenant,
}

impl SynthesisTierKind {
    /// Stable wire tag used in status records.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Domain => "domain",
            Self::Tenant => "tenant",
        }
    }
}

/// Status record for one synthesis window, returned by
/// [`crate::synthesis::synthesis_status`] and
/// [`crate::synthesis::list_recent_syntheses`].
///
/// All fields are wire-flat: UUID strings, `i64` Unix epoch
/// timestamps, and a `String`-tagged status field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, uniffi::Record)]
#[serde(rename_all = "camelCase")]
pub struct SynthesisStatusRecord {
    /// UUID-string synthesis window id.
    pub synthesis_id: String,
    /// UUID-string scope id.
    pub scope_id: String,
    /// Tier tag: `"domain"` or `"tenant"`.
    pub tier: String,
    /// Lifecycle status: `"pending"`, `"in_progress"`,
    /// `"complete"`, or `"failed"`.
    pub status: String,
    /// Unix epoch seconds for the inclusive window start.
    pub window_start_unix: i64,
    /// Unix epoch seconds for the exclusive window end.
    pub window_end_unix: i64,
    /// UUID-string synthesis object id, present once the window
    /// has transitioned to `Complete` and the synthesis object
    /// has been persisted.
    pub object_id: Option<String>,
    /// Monotonically increasing version stamp of the synthesis
    /// object currently associated with the window. The original
    /// dispatch produces `version = 1`; each `replay_synthesis`
    /// call on the same window bumps the stamp by 1.
    ///
    /// Present whenever `object_id` is — both come from the
    /// underlying [`synthesis_pipeline::SynthesisObject`]. Hosts
    /// can use this to detect that a previously cached recap is
    /// stale relative to a new replay landing.
    pub object_version: Option<u32>,
}

/// One entry in the per-window synthesis-version history returned
/// by [`crate::synthesis::list_synthesis_versions`]. The latest
/// version exposed via [`SynthesisStatusRecord::object_version`]
/// is **also** included in this list (so a host can paginate the
/// history without a separate "current" round trip).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, uniffi::Record)]
#[serde(rename_all = "camelCase")]
pub struct SynthesisVersionSummary {
    /// Monotonically increasing version stamp (1-based).
    pub version: u32,
    /// Unix epoch seconds at which the version was archived to
    /// the history table. For the *current* latest version the
    /// timestamp is read from the live `SynthesisObject`'s
    /// `created_at`; for prior versions it is the archive time
    /// recorded by `save_synthesis_object_version`.
    pub created_at_unix: i64,
    /// Synthesis object kind tag (`"domain_summary"` /
    /// `"tenant_summary"`) so a host UI can render the history
    /// without re-loading every payload.
    pub object_type: String,
    /// `true` if this is the latest version (the one a host
    /// would receive from `synthesis_status` / the in-memory
    /// `synthesis_objects` map). At most one entry in any list
    /// returned by `list_synthesis_versions` has this flag set.
    pub is_latest: bool,
}

/// Configuration for the server-side synthesis engine endpoint.
///
/// Forwarded through to
/// [`synthesis_engine::EndpointConfig`] inside
/// [`crate::synthesis::configure_synthesis_engine`]. `api_key_ref`
/// is **not** the raw API key — it is the name of an environment
/// variable holding the cleartext token (or a host-resolved key
/// reference). The substrate never stores the cleartext token.
// `Eq` is intentionally NOT derived because `rate_refill_per_sec`
// is `f64`. The serde round-trip test uses `assert_eq!` on the
// struct, which `PartialEq` is sufficient for — `NaN` values
// would already break round-trip elsewhere (JSON has no NaN
// representation), so the lost reflexivity is not a real
// regression. See the user-knowledge note on f64-on-serde
// structs in this repo's session memory.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, uniffi::Record)]
#[serde(rename_all = "camelCase")]
pub struct SynthesisEngineConfig {
    /// HTTPS URL of the synthesis endpoint.
    pub url: String,
    /// Secret-store reference for the API key (NOT the raw key).
    /// Hosts MUST set the corresponding env var before calling
    /// `configure_synthesis_engine` or the first dispatch will
    /// fail with `EndpointError::InvalidRequest`.
    pub api_key_ref: String,
    /// Model identifier (e.g. `"slm-recap-v1"`).
    pub model_id: String,
    /// Response token cap. `0` falls back to
    /// [`synthesis_engine::DEFAULT_MAX_TOKENS`].
    pub max_tokens: u32,
    /// Per-request timeout in milliseconds. `0` falls back to
    /// [`synthesis_engine::DEFAULT_TIMEOUT`]. Values exceeding
    /// [`crate::synthesis::MAX_TIMEOUT_MS`] (10 min) are rejected by
    /// `configure_synthesis_engine` with `FfiError::Unavailable` —
    /// `Duration::from_millis(u64::MAX)` would otherwise disable
    /// the timeout entirely and let a wedged endpoint hold the
    /// dispatch thread.
    pub timeout_ms: u64,
    /// Optional GBNF grammar for constrained decoding.
    pub grammar: Option<String>,
    /// Allow-list of UUID strings the configured non-TEE engine
    /// is permitted to operate on. `None` disables the
    /// FFI-layer scope-binding check (a tracing warning is logged
    /// on every dispatch); `Some(empty)` is a hard refusal of
    /// every scope, matching the TEE worker's `scope_bindings`
    /// semantics.
    pub scope_bindings: Option<Vec<String>>,
    /// When `true`, the synthesis health-probe reports `Nominal`
    /// instead of `Degraded` when `scope_bindings` is absent.
    /// Single-tenant / dev deployments do not require scope
    /// enforcement, so the `Degraded` signal is noise in those
    /// environments. Defaults to `false` for back-compat with
    /// the multi-tenant production posture.
    #[serde(default)]
    pub single_tenant: bool,

    /// Per-endpoint requests-per-minute cap for the synthesis
    /// rate limiter. `None` uses the library default
    /// ([`synthesis_engine::DEFAULT_MAX_RPM`], 60). Set to
    /// `Some(n)` for enterprise-negotiated higher caps, or use a
    /// very large value (e.g. `u64::MAX`) to effectively disable
    /// rate limiting. `Some(0)` is rejected as invalid because a
    /// zero-cap limiter blocks every request.
    #[serde(default)]
    pub max_requests_per_minute: Option<u64>,

    /// Burst capacity for the global rate-shaping token bucket
    /// gating [`crate::synthesis::trigger_server_synthesis`].
    /// `0` falls back to
    /// [`crate::synthesis::DEFAULT_TRIGGER_RATE_CAPACITY`] (8) —
    /// the same sentinel-zero pattern used by `max_tokens` and
    /// `timeout_ms`. Hosts that want to disable rate-shaping
    /// outright cannot — the bucket is always on so a
    /// misconfigured host can never race past the cap. Use a
    /// large value (e.g. `1_000`) to effectively disable.
    #[serde(default)]
    pub rate_capacity: u32,

    /// Refill rate (tokens per second) for the global
    /// rate-shaping token bucket gating
    /// [`crate::synthesis::trigger_server_synthesis`].
    /// `0.0` falls back to
    /// [`crate::synthesis::DEFAULT_TRIGGER_RATE_REFILL_PER_SEC`]
    /// (1.0). Fractional values are supported (e.g. `0.5` ==
    /// one token every 2 seconds). Negative values are rejected
    /// by [`crate::synthesis::configure_synthesis_engine`] with
    /// `FfiError::Unavailable`.
    #[serde(default)]
    pub rate_refill_per_sec: f64,
}

/// Summary view of an approved-document reference + payload pair,
/// returned by [`crate::synthesis::admit_approved_document`] and
/// [`crate::synthesis::list_approved_documents`].
///
/// All fields are wire-flat: UUID strings, an `i64` Unix-epoch-ms
/// timestamp, a hex content hash, and a `u64` byte size. The
/// substrate never surfaces the raw payload through this surface
/// (hosts that admitted it already own the original bytes); only
/// `payload_bytes` and `content_hash_hex` are exposed so a host
/// can correlate the on-disk row with the bytes it sent.
///
/// `payload_bytes == 0` and `content_hash_hex.is_empty()` indicate
/// a tenant-memory ref that has no corresponding evidence-store
/// payload row. Under normal operation this should only occur transiently
/// (e.g. between a host's call to `admit_approved_document` and a
/// crash before tenant memory was flushed), but the substrate
/// surfaces it explicitly rather than synthesising fake metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, uniffi::Record)]
#[serde(rename_all = "camelCase")]
pub struct ApprovedDocumentSummary {
    /// UUID-string document id (matches the
    /// `memory_manager::ApprovedDocumentRef::id` field).
    pub id: String,
    /// UUID-string tenant scope the document is admitted onto.
    pub scope_id: ScopeIdString,
    /// Stable label / title (e.g. `"Tenant Policy v3.2"`).
    pub label: String,
    /// Free-form approver reference (e.g. `"compliance-officer"`).
    pub approver: String,
    /// Wall-clock approval time in Unix epoch milliseconds.
    pub approved_at_ms: i64,
    /// Plaintext payload size in bytes (NOT the AEAD ciphertext
    /// size). `0` when the tenant-memory ref has no corresponding
    /// evidence-store payload row (see struct-level docs).
    pub payload_bytes: u64,
    /// BLAKE3 content hash of the plaintext payload, lower-hex
    /// (64 chars). Empty string when the tenant-memory ref has no
    /// corresponding evidence-store payload row.
    pub content_hash_hex: String,
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
            language_tag: Some("en".into()),
        };
        let s = serde_json::to_string(&r).unwrap();
        let back: EvidenceRecord = serde_json::from_str(&s).unwrap();
        assert_eq!(r, back);
    }

    /// Schema-v13 backward-compat: an
    /// `EvidenceRecord` JSON blob emitted by a pre-v13 host
    /// bridge — i.e. one that doesn't know about the
    /// `language_tag` key — must still deserialise. The
    /// `#[serde(default)]` attribute on the field makes the
    /// absent-key case resolve to `None` rather than failing
    /// with `missing field`.
    #[test]
    fn evidence_record_deserialises_pre_v13_payload_without_language_tag() {
        let pre_v13 = r#"{
            "id": "00000000-0000-0000-0000-000000000001",
            "scope_id": "00000000-0000-0000-0000-000000000002",
            "body": "hello world",
            "source": "Slack",
            "created_at": 1700000000
        }"#;
        let r: EvidenceRecord = serde_json::from_str(pre_v13).unwrap();
        assert_eq!(r.language_tag, None);
        assert_eq!(r.id, "00000000-0000-0000-0000-000000000001");
        assert_eq!(r.body, "hello world");
    }

    /// `EvidenceRecord` carries the BCP-47 tag end-to-end so
    /// host bindings (Swift / Kotlin / Electron) can pick a
    /// per-locale render pipeline without re-running detection
    /// on the read side. NULL must stay NULL across the bridge.
    #[test]
    fn evidence_record_preserves_language_tag_round_trip() {
        for tag in [
            None,
            Some("en".to_string()),
            Some("ja".to_string()),
            Some("zh".to_string()),
        ] {
            let r = EvidenceRecord {
                id: "00000000-0000-0000-0000-000000000001".into(),
                scope_id: "00000000-0000-0000-0000-000000000002".into(),
                body: "round trip".into(),
                source: SourceKind::Slack,
                created_at: 1_700_000_000,
                language_tag: tag.clone(),
            };
            let s = serde_json::to_string(&r).unwrap();
            let back: EvidenceRecord = serde_json::from_str(&s).unwrap();
            assert_eq!(back.language_tag, tag);
        }
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

    /// Pin the JSON wire format `crates/napi/src/bindings.rs::
    /// js_sync_scheduler_status` documents — every key MUST be the
    /// camelCase form documented in the rustdoc (`isRunning`,
    /// `startedAtUnix`, …). The regression
    /// was that the doc promised camelCase but the type derived
    /// `Serialize` without `rename_all`, producing snake_case keys
    /// that would surface as `undefined` when destructured by a JS
    /// caller following the documented pattern.
    #[test]
    fn sync_scheduler_status_serializes_with_camelcase_keys() {
        let status = SyncSchedulerStatus {
            is_running: true,
            started_at_unix: Some(1_700_000_000),
            default_interval_secs: 900,
            default_max_backoff_secs: 28_800,
            tick_interval_secs: 30,
            policy_override_count: 3,
            total_instance_count: 11,
            last_tick_at_unix: Some(1_700_000_030),
            ticks_completed: 12,
            dispatches_attempted: 7,
            dispatches_succeeded: 6,
            dispatches_failed: 1,
            dispatches_skipped_in_progress: 0,
        };
        let v = serde_json::to_value(&status).expect("serialize");
        let obj = v.as_object().expect("object");
        // Every documented camelCase key MUST appear; every
        // snake_case key MUST NOT.
        for camel in [
            "isRunning",
            "startedAtUnix",
            "defaultIntervalSecs",
            "defaultMaxBackoffSecs",
            "tickIntervalSecs",
            "policyOverrideCount",
            "totalInstanceCount",
            "lastTickAtUnix",
            "ticksCompleted",
            "dispatchesAttempted",
            "dispatchesSucceeded",
            "dispatchesFailed",
            "dispatchesSkippedInProgress",
        ] {
            assert!(
                obj.contains_key(camel),
                "SyncSchedulerStatus JSON must contain camelCase key `{camel}`; got {v}"
            );
        }
        for snake in [
            "is_running",
            "started_at_unix",
            "default_interval_secs",
            "default_max_backoff_secs",
            "tick_interval_secs",
            "policy_override_count",
            "total_instance_count",
            "last_tick_at_unix",
            "ticks_completed",
            "dispatches_attempted",
            "dispatches_succeeded",
            "dispatches_failed",
            "dispatches_skipped_in_progress",
        ] {
            assert!(
                !obj.contains_key(snake),
                "SyncSchedulerStatus JSON must NOT contain snake_case key `{snake}`; got {v}"
            );
        }
        // Pin that the two count fields
        // serialize with the post-rename camelCase keys and that
        // their values come through distinct from each other (so a
        // future refactor that conflates them in code is caught
        // here, not by a host reporting wrong telemetry).
        assert_eq!(obj.get("policyOverrideCount").and_then(serde_json::Value::as_u64),
            Some(3),
            "policyOverrideCount must serialize as the configured value, distinct from totalInstanceCount"
        );
        assert_eq!(obj.get("totalInstanceCount").and_then(serde_json::Value::as_u64),
            Some(11),
            "totalInstanceCount must serialize as the configured value, distinct from policyOverrideCount"
        );
        // Round-trip through Deserialize too — guarantees the
        // camelCase rename applies symmetrically.
        let back: SyncSchedulerStatus = serde_json::from_value(v).expect("deserialize");
        assert_eq!(back, status);
    }

    /// `WebhookServerSummary` had the same latent wire
    /// format mismatch (doc at `crates/napi/src/bindings.rs::
    /// js_list_webhook_servers` documents camelCase but the type
    /// originally derived `Serialize` without `rename_all`). Pin the
    /// post-fix shape so a future change to either the type or the
    /// doc is caught here.
    #[test]
    fn webhook_server_summary_serializes_with_camelcase_keys() {
        let summary = WebhookServerSummary {
            server_handle: WebhookServerHandle(42),
            bind_addr: "127.0.0.1:9001".into(),
            started_at: 1_700_000_000,
            registration_count: 2,
            dispatch_ok_total: 10,
            dispatch_bad_request_total: 1,
            dispatch_bad_gateway_total: 0,
        };
        let v = serde_json::to_value(&summary).expect("serialize");
        let obj = v.as_object().expect("object");
        for camel in [
            "serverHandle",
            "bindAddr",
            "startedAt",
            "registrationCount",
            "dispatchOkTotal",
            "dispatchBadRequestTotal",
            "dispatchBadGatewayTotal",
        ] {
            assert!(
                obj.contains_key(camel),
                "WebhookServerSummary JSON must contain camelCase key `{camel}`; got {v}"
            );
        }
        for snake in [
            "server_handle",
            "bind_addr",
            "started_at",
            "registration_count",
            "dispatch_ok_total",
            "dispatch_bad_request_total",
            "dispatch_bad_gateway_total",
        ] {
            assert!(
                !obj.contains_key(snake),
                "WebhookServerSummary JSON must NOT contain snake_case key `{snake}`; got {v}"
            );
        }
        let back: WebhookServerSummary = serde_json::from_value(v).expect("deserialize");
        assert_eq!(back, summary);
    }

    /// `RefreshReport` is the response envelope for
    /// `crates/napi/src/bindings.rs::js_refresh_connector_token`,
    /// whose rustdoc documents `{ instanceId, refreshed, expiresAt,
    /// refreshedAt }` (camelCase). A later review flagged the
    /// type as still serializing snake_case keys despite the doc
    /// claim; the rename now aligns the wire format with the doc.
    /// Pin the camelCase invariant so future drift between doc and
    /// type is caught at `cargo test` time.
    #[test]
    fn refresh_report_serializes_with_camelcase_keys() {
        let r = RefreshReport {
            instance_id: "00000000-0000-0000-0000-000000000001".into(),
            refreshed: true,
            expires_at: 1_900_000_000,
            refreshed_at: 1_800_000_000,
        };
        let v = serde_json::to_value(&r).expect("serialize");
        let obj = v.as_object().expect("object");
        for camel in ["instanceId", "refreshed", "expiresAt", "refreshedAt"] {
            assert!(
                obj.contains_key(camel),
                "RefreshReport JSON must contain camelCase key `{camel}`; got {v}"
            );
        }
        for snake in ["instance_id", "expires_at", "refreshed_at"] {
            assert!(
                !obj.contains_key(snake),
                "RefreshReport JSON must NOT contain snake_case key `{snake}`; got {v}"
            );
        }
        let back: RefreshReport = serde_json::from_value(v).expect("deserialize");
        assert_eq!(back, r);
    }

    /// `SyncReport` is the response envelope for
    /// `crates/napi/src/bindings.rs::js_sync_connector`. The
    /// `rename_all` rename keeps it consistent with every other
    /// N-API JSON-returning entry point.
    #[test]
    fn sync_report_serializes_with_camelcase_keys() {
        let r = SyncReport {
            instance_id: "00000000-0000-0000-0000-000000000001".into(),
            mode: SyncModeKind::Incremental,
            events_total: 42,
            events_ingested: 40,
            ingested_evidence_ids: vec![
                "00000000-0000-0000-0000-0000000000aa".into(),
                "00000000-0000-0000-0000-0000000000bb".into(),
            ],
            next_cursor: Some("cursor-token".into()),
            started_at: 1_700_000_000,
            completed_at: 1_700_000_005,
        };
        let v = serde_json::to_value(&r).expect("serialize");
        let obj = v.as_object().expect("object");
        for camel in [
            "instanceId",
            "mode",
            "eventsTotal",
            "eventsIngested",
            "ingestedEvidenceIds",
            "nextCursor",
            "startedAt",
            "completedAt",
        ] {
            assert!(
                obj.contains_key(camel),
                "SyncReport JSON must contain camelCase key `{camel}`; got {v}"
            );
        }
        for snake in [
            "instance_id",
            "events_total",
            "events_ingested",
            "ingested_evidence_ids",
            "next_cursor",
            "started_at",
            "completed_at",
        ] {
            assert!(
                !obj.contains_key(snake),
                "SyncReport JSON must NOT contain snake_case key `{snake}`; got {v}"
            );
        }
        // Nested `SyncModeKind` enum keeps its independent
        // `rename_all = "snake_case"` discipline — the tag value
        // for `Incremental` is the lowercase string `"incremental"`,
        // NOT the camelCase `"Incremental"` the struct-level rename
        // would suggest. The struct-level `rename_all` only
        // governs field names, not nested enum variants.
        assert_eq!(
            obj.get("mode").and_then(|m| m.as_str()),
            Some("incremental"),
            "SyncModeKind variant tag must remain snake_case"
        );
        let back: SyncReport = serde_json::from_value(v).expect("deserialize");
        assert_eq!(back, r);
    }

    /// `ConnectorStatus` is the response envelope for
    /// `crates/napi/src/bindings.rs::js_list_connectors`. An earlier
    /// review flagged it as the last `serde_json`-serialized N-API
    /// return type still emitting
    /// snake_case keys (every peer — `RefreshReport`,
    /// `SyncReport`, `WebhookServerSummary`,
    /// `SyncSchedulerStatus` — has been migrated). Pin the
    /// camelCase invariant so future drift between the
    /// documented JS-idiomatic surface and the type is caught at
    /// `cargo test` time.
    #[test]
    fn connector_status_serializes_with_camelcase_keys() {
        let s = ConnectorStatus {
            instance_id: "00000000-0000-0000-0000-000000000001".into(),
            kind: ConnectorKindTag::GoogleDrive,
            scope_id: "00000000-0000-0000-0000-0000000000aa".into(),
            sync_mode: SyncModeKind::Incremental,
            sync_status: SyncStatusKind::Succeeded,
            last_synced_at: Some(1_700_000_000),
            last_error: None,
        };
        let v = serde_json::to_value(&s).expect("serialize");
        let obj = v.as_object().expect("object");
        for camel in [
            "instanceId",
            "kind",
            "scopeId",
            "syncMode",
            "syncStatus",
            "lastSyncedAt",
            "lastError",
        ] {
            assert!(
                obj.contains_key(camel),
                "ConnectorStatus JSON must contain camelCase key `{camel}`; got {v}"
            );
        }
        for snake in [
            "instance_id",
            "scope_id",
            "sync_mode",
            "sync_status",
            "last_synced_at",
            "last_error",
        ] {
            assert!(
                !obj.contains_key(snake),
                "ConnectorStatus JSON must NOT contain snake_case key `{snake}`; got {v}"
            );
        }
        // Nested enums keep their independent
        // `rename_all = "snake_case"` discipline — the struct-level
        // camelCase rename governs field names only, not the tag
        // values of nested enums. Pin both nested tags so a future
        // refactor that flips an enum's own `rename_all` to
        // `camelCase` (which would silently break every JS
        // consumer matching `"google_drive"`, `"incremental"`,
        // `"succeeded"`) gets caught here.
        assert_eq!(
            obj.get("kind").and_then(|m| m.as_str()),
            Some("google_drive"),
            "ConnectorKindTag variant tag must remain snake_case"
        );
        assert_eq!(
            obj.get("syncMode").and_then(|m| m.as_str()),
            Some("incremental"),
            "SyncModeKind variant tag must remain snake_case"
        );
        assert_eq!(
            obj.get("syncStatus").and_then(|m| m.as_str()),
            Some("succeeded"),
            "SyncStatusKind variant tag must remain snake_case"
        );
        let back: ConnectorStatus = serde_json::from_value(v).expect("deserialize");
        assert_eq!(back, s);
    }

    /// `ConnectorHealthRecord` is the new single-instance probe
    /// envelope returned by
    /// [`crate::connector::connector_status`], symmetric with
    /// [`crate::synthesis::synthesis_status`]. Pin the camelCase
    /// invariant on every field of the wire format so a future
    /// drift between the documented JS-idiomatic surface (read
    /// by N-API hosts directly off the JSON object) and the Rust
    /// type's `#[serde(rename_all = "camelCase")]` is caught at
    /// `cargo test` time.
    ///
    /// Also pins the round-trip `assert_eq!(back, s)` so a
    /// regression that drops a field (or changes a default) is
    /// caught — `ConnectorHealthRecord` has no `#[serde(default)]`
    /// fields today, so any missing key on deserialize would
    /// surface as a panic.
    #[test]
    fn connector_health_record_serializes_with_camelcase_keys() {
        let s = ConnectorHealthRecord {
            instance_id: "00000000-0000-0000-0000-000000000001".into(),
            kind: ConnectorKindTag::GoogleDrive,
            scope_id: "00000000-0000-0000-0000-0000000000aa".into(),
            sync_mode: SyncModeKind::Incremental,
            sync_status: SyncStatusKind::Failed,
            last_synced_at: Some(1_700_000_000),
            last_error: Some("HTTP 503 from googleapis.com".into()),
            is_scheduled: true,
            sync_interval_secs: 900,
            max_backoff_secs: 28_800,
            auto_synthesize: true,
            consecutive_failures: 3,
            next_attempt_unix: Some(1_700_010_000),
            in_cooldown: true,
        };
        let v = serde_json::to_value(&s).expect("serialize");
        let obj = v.as_object().expect("object");
        for camel in [
            "instanceId",
            "kind",
            "scopeId",
            "syncMode",
            "syncStatus",
            "lastSyncedAt",
            "lastError",
            "isScheduled",
            "syncIntervalSecs",
            "maxBackoffSecs",
            "autoSynthesize",
            "consecutiveFailures",
            "nextAttemptUnix",
            "inCooldown",
        ] {
            assert!(
                obj.contains_key(camel),
                "ConnectorHealthRecord JSON must contain camelCase key `{camel}`; got {v}"
            );
        }
        for snake in [
            "instance_id",
            "scope_id",
            "sync_mode",
            "sync_status",
            "last_synced_at",
            "last_error",
            "is_scheduled",
            "sync_interval_secs",
            "max_backoff_secs",
            "auto_synthesize",
            "consecutive_failures",
            "next_attempt_unix",
            "in_cooldown",
        ] {
            assert!(
                !obj.contains_key(snake),
                "ConnectorHealthRecord JSON must NOT contain snake_case key `{snake}`; got {v}"
            );
        }
        // Nested-enum tag pins — same defense-in-depth as
        // `connector_status_serializes_with_camelcase_keys`. A
        // future refactor that flips `ConnectorKindTag` /
        // `SyncModeKind` / `SyncStatusKind` from `snake_case` to
        // `camelCase` would silently break every JS consumer
        // matching on `"google_drive"` / `"incremental"` /
        // `"failed"`.
        assert_eq!(
            obj.get("kind").and_then(|m| m.as_str()),
            Some("google_drive"),
            "ConnectorKindTag variant tag must remain snake_case"
        );
        assert_eq!(
            obj.get("syncMode").and_then(|m| m.as_str()),
            Some("incremental"),
            "SyncModeKind variant tag must remain snake_case"
        );
        assert_eq!(
            obj.get("syncStatus").and_then(|m| m.as_str()),
            Some("failed"),
            "SyncStatusKind variant tag must remain snake_case"
        );
        let back: ConnectorHealthRecord = serde_json::from_value(v).expect("deserialize");
        assert_eq!(back, s);
    }

    #[test]
    fn synthesis_tier_kind_round_trips_via_serde() {
        for kind in [SynthesisTierKind::Domain, SynthesisTierKind::Tenant] {
            let serialized = serde_json::to_string(&kind).expect("serialize");
            // snake_case discipline keeps platform JSON decoders happy.
            assert!(
                serialized == "\"domain\"" || serialized == "\"tenant\"",
                "SynthesisTierKind must serialize as snake_case: {serialized}"
            );
            let back: SynthesisTierKind = serde_json::from_str(&serialized).expect("deserialize");
            assert_eq!(back, kind);
        }
    }

    #[test]
    fn synthesis_tier_kind_as_str_matches_wire_tag() {
        assert_eq!(SynthesisTierKind::Domain.as_str(), "domain");
        assert_eq!(SynthesisTierKind::Tenant.as_str(), "tenant");
    }

    #[test]
    fn synthesis_status_record_serializes_with_camelcase_keys() {
        let record = SynthesisStatusRecord {
            synthesis_id: "11111111-1111-1111-1111-111111111111".into(),
            scope_id: "22222222-2222-2222-2222-222222222222".into(),
            tier: "domain".into(),
            status: "complete".into(),
            window_start_unix: 1_000,
            window_end_unix: 2_000,
            object_id: Some("33333333-3333-3333-3333-333333333333".into()),
            object_version: Some(2),
        };
        let v = serde_json::to_value(&record).expect("serialize");
        let obj = v.as_object().expect("object");
        for camel in [
            "synthesisId",
            "scopeId",
            "tier",
            "status",
            "windowStartUnix",
            "windowEndUnix",
            "objectId",
            "objectVersion",
        ] {
            assert!(
                obj.contains_key(camel),
                "SynthesisStatusRecord JSON must contain camelCase key `{camel}`: {v}"
            );
        }
        for snake in [
            "synthesis_id",
            "scope_id",
            "window_start_unix",
            "window_end_unix",
            "object_id",
            "object_version",
        ] {
            assert!(
                !obj.contains_key(snake),
                "SynthesisStatusRecord JSON must NOT contain snake_case key `{snake}`: {v}"
            );
        }
        let back: SynthesisStatusRecord = serde_json::from_value(v).expect("deserialize");
        assert_eq!(back, record);
    }

    #[test]
    fn synthesis_version_summary_serializes_with_camelcase_keys() {
        let summary = SynthesisVersionSummary {
            version: 3,
            created_at_unix: 1_700_000_000,
            object_type: "domain_summary".into(),
            is_latest: true,
        };
        let v = serde_json::to_value(&summary).expect("serialize");
        let obj = v.as_object().expect("object");
        for camel in ["version", "createdAtUnix", "objectType", "isLatest"] {
            assert!(
                obj.contains_key(camel),
                "SynthesisVersionSummary JSON must contain camelCase key `{camel}`: {v}"
            );
        }
        for snake in ["created_at_unix", "object_type", "is_latest"] {
            assert!(
                !obj.contains_key(snake),
                "SynthesisVersionSummary JSON must NOT contain snake_case key `{snake}`: {v}"
            );
        }
        let back: SynthesisVersionSummary = serde_json::from_value(v).expect("deserialize");
        assert_eq!(back, summary);
    }

    #[test]
    fn synthesis_engine_config_round_trips_via_serde() {
        let cfg = SynthesisEngineConfig {
            url: "https://api.example/synth".into(),
            api_key_ref: "SYNTH_KEY_REF".into(),
            model_id: "slm-recap-v1".into(),
            max_tokens: 1024,
            timeout_ms: 30_000,
            grammar: Some("root ::= object".into()),
            scope_bindings: Some(vec![
                "44444444-4444-4444-4444-444444444444".into(),
                "55555555-5555-5555-5555-555555555555".into(),
            ]),
            single_tenant: false,
            max_requests_per_minute: Some(120),
            rate_capacity: 16,
            rate_refill_per_sec: 2.5,
        };
        let v = serde_json::to_value(&cfg).expect("serialize");
        let obj = v.as_object().expect("object");
        for camel in [
            "url",
            "apiKeyRef",
            "modelId",
            "maxTokens",
            "timeoutMs",
            "grammar",
            "scopeBindings",
            "singleTenant",
            "maxRequestsPerMinute",
            "rateCapacity",
            "rateRefillPerSec",
        ] {
            assert!(
                obj.contains_key(camel),
                "SynthesisEngineConfig JSON must contain camelCase key `{camel}`: {v}"
            );
        }
        for snake in [
            "api_key_ref",
            "model_id",
            "max_tokens",
            "timeout_ms",
            "scope_bindings",
            "single_tenant",
            "max_requests_per_minute",
            "rate_capacity",
            "rate_refill_per_sec",
        ] {
            assert!(
                !obj.contains_key(snake),
                "SynthesisEngineConfig JSON must NOT contain snake_case key `{snake}`: {v}"
            );
        }
        let back: SynthesisEngineConfig = serde_json::from_value(v).expect("deserialize");
        assert_eq!(back, cfg);
    }

    #[test]
    fn synthesis_engine_config_grammar_is_optional() {
        let cfg = SynthesisEngineConfig {
            url: "https://api.example/synth".into(),
            api_key_ref: "SYNTH_KEY_REF".into(),
            model_id: "slm-recap-v1".into(),
            max_tokens: 0,
            timeout_ms: 0,
            grammar: None,
            scope_bindings: None,
            single_tenant: false,
            max_requests_per_minute: None,
            rate_capacity: 0,
            rate_refill_per_sec: 0.0,
        };
        let back: SynthesisEngineConfig =
            serde_json::from_str(&serde_json::to_string(&cfg).expect("serialize"))
                .expect("deserialize");
        assert_eq!(back, cfg);
    }
}
