//! `knowledge_napi` — N-API addon skeleton for macOS / Windows
//! Electron desktop integration.
//!
//! Per `ARCHITECTURE.md` §3 ("Platform integration plane") and
//! `docs/DESIGN.md` §2 ("On-device runtime"), the desktop bridge ships
//! as a Node.js native addon that mirrors the iOS / Android UniFFI
//! surface (see the sibling `ffi` crate) but speaks JSON-over-N-API
//! instead of typed object handles.
//!
//! The current deliverable is the **wire skeleton**:
//!
//! 1. JSON-shaped wrapper types (re-exported from `ffi::types` with
//!    a desktop-only [`InitConfig`] added).
//! 2. Function signatures matching the contract in `crate::ffi` —
//!    every call takes `serde_json::Value` arguments (because that
//!    is exactly how `napi-derive` will serialize the JS-side object
//!    arguments) and returns `serde_json::Value` on success.
//! 3. A round-trippable [`NapiError`] mapped from [`ffi::FfiError`]
//!    so the Electron host gets a stable JSON envelope.
//!
//! Phase 4 — the [`#[napi]`] proc-macros are live. The cdylib that
//! `napi build` produces is loaded by Node via `require('./*.node')`.
//! The [`bindings`] module is the JS-facing surface; the freestanding
//! `pub fn`s in this file remain the canonical Rust-facing API so
//! unit tests and Rust callers can exercise the substrate without
//! going through the Node bridge.
//!
//! See [`bindings`] for the full JS API.

#![deny(missing_docs)]
// Most N-API entry points in this file forward their `String` /
// `Vec<u8>` arguments straight into the matching `ffi::*` call, which
// consumes them by value — clippy treats that as a genuine
// consumption and does not fire `needless_pass_by_value`. The
// exception is the `encrypt` / `decrypt` pair: they call helpers
// that only borrow their inputs, so a per-function
// `#[allow(clippy::needless_pass_by_value)]` is applied there with a
// comment explaining why the by-value signature is kept (napi-derive
// hands owned `String` / `Vec<u8>` across the JS boundary on every
// call; borrowing would force an extra copy in generated code).
// Keeping the allows local lets clippy still catch inadvertent
// by-value taking in internal helpers that don't cross the FFI
// boundary.

pub mod bindings;
pub mod error;
pub mod types;

pub use error::{NapiError, NapiResult};
#[cfg(feature = "tracing-subscriber")]
pub use ffi::try_init_tracing;
pub use ffi::{
    AdapterReport, ApprovedDocumentSummary, ConnectorKindTag, ConnectorStatus, EvidenceRecord,
    FfiImportanceClass, FfiKeypair, FfiSignature, HealthStatus, MemoryFilter, MemoryRecord,
    MemoryState, MetricsSnapshot, QueryResult, RefreshReport, RuntimeHandle, ScopeIdString,
    SourceKind, SubsystemHealth, SubsystemStatus, SyncModeKind, SyncReport, SyncSchedulerStatus,
    SyncStatusKind, SynthesisTrigger,
};
pub use types::{IngestRequest, InitConfig, QueryRequest};

/// Wire-stable handle to an open store. Hosts receive this from
/// [`open_store`] and must pass it back into every subsequent call.
/// JavaScript represents this as a `bigint` to preserve the full
/// 64-bit width without loss of precision (N-API will marshal this
/// transparently once the `#[napi]` macros land).
///
/// # Sentinel
///
/// `0n` (BigInt zero) is the reserved "no handle" sentinel mirroring
/// [`RuntimeHandle::NONE`]. The handle allocator on the Rust side
/// starts at `1n` and never re-mints `0n`, so any call from JS that
/// passes `0n` is guaranteed to be rejected with the
/// `"Unavailable"` kind tag — surfaced as
/// [`NapiError::Ffi`]`(`[`ffi::FfiError::Unavailable`]`)` and
/// stringified to JS as `kind: "Unavailable"` by
/// [`NapiError::kind`] (which delegates through the wrapped
/// [`ffi::FfiError`]). Hosts should treat `0n` as "not yet opened"
/// rather than as a valid handle.
pub type NapiHandle = u64;

/// Initialize the Rust core with a JSON config blob. Hosts call this
/// once during Electron's `app.whenReady` hook.
///
/// # Errors
///
/// Returns [`NapiError::InvalidConfig`] if `config_json` is not valid
/// JSON or does not match [`InitConfig`].
pub fn init(config_json: &str) -> NapiResult<()> {
    let _cfg: InitConfig =
        serde_json::from_str(config_json).map_err(|e| NapiError::InvalidConfig {
            message: e.to_string(),
        })?;
    Ok(())
}

/// Open the SQLCipher-backed evidence store at `path` using the
/// 32-byte master key encoded as `master_key_hex` (64 lower-case hex
/// chars). Mirrors [`ffi::open_store`]. Returns the allocated
/// [`NapiHandle`] the host must pass back into every subsequent
/// call.
///
/// # Errors
///
/// Forwards [`ffi::open_store`] errors as [`NapiError`].
pub fn open_store(path: String, master_key_hex: String) -> NapiResult<NapiHandle> {
    ffi::open_store(path, master_key_hex)
        .map(|h| h.0)
        .map_err(NapiError::from)
}

/// Drop the open evidence store identified by `handle`. Mirrors
/// [`ffi::close_store`]. Calling with an unknown handle is a no-op
/// — hosts may invoke this in `try`/`finally` shutdown paths.
///
/// # Errors
///
/// Forwards [`ffi::close_store`] errors as [`NapiError`].
pub fn close_store(handle: NapiHandle) -> NapiResult<()> {
    ffi::close_store(RuntimeHandle(handle)).map_err(NapiError::from)
}

/// Ingest a chat / document message through the encrypted evidence
/// plane. Mirrors [`ffi::ingest_message`].
///
/// # Errors
///
/// Returns [`NapiError`] if the request body is malformed or the
/// underlying FFI surface returns an error.
pub fn ingest_message(handle: NapiHandle, req: IngestRequest) -> NapiResult<serde_json::Value> {
    ffi::ingest_message(
        RuntimeHandle(handle),
        req.scope_id,
        req.body,
        req.source,
        req.importance,
    )
    .map(|id| serde_json::json!({ "evidence_id": id }))
    .map_err(NapiError::from)
}

/// Hybrid query against a scope. Mirrors [`ffi::query`].
///
/// # Errors
///
/// Forwards [`ffi::query`] errors as [`NapiError`].
pub fn query(handle: NapiHandle, req: QueryRequest) -> NapiResult<Vec<QueryResult>> {
    ffi::query(
        RuntimeHandle(handle),
        req.scope_id,
        req.query_text,
        req.limit,
    )
    .map_err(NapiError::from)
}

/// Fetch a single evidence row. Mirrors [`ffi::get_evidence`].
///
/// # Errors
///
/// Forwards [`ffi::get_evidence`] errors as [`NapiError`].
pub fn get_evidence(handle: NapiHandle, evidence_id: String) -> NapiResult<EvidenceRecord> {
    ffi::get_evidence(RuntimeHandle(handle), evidence_id).map_err(NapiError::from)
}

/// Fetch the per-user memory bundle for a scope.
///
/// # Errors
///
/// Forwards [`ffi::get_user_memory`] errors as [`NapiError`].
pub fn get_user_memory(
    handle: NapiHandle,
    scope_id: ScopeIdString,
) -> NapiResult<Vec<MemoryRecord>> {
    ffi::get_user_memory(RuntimeHandle(handle), scope_id).map_err(NapiError::from)
}

/// Mark a memory record as `Pinned`.
///
/// # Errors
///
/// Forwards [`ffi::pin`] errors as [`NapiError`].
pub fn pin(handle: NapiHandle, id: String) -> NapiResult<()> {
    ffi::pin(RuntimeHandle(handle), id).map_err(NapiError::from)
}

/// Lift a previously-applied pin so the row resumes ageing.
///
/// # Errors
///
/// Forwards [`ffi::unpin`] errors as [`NapiError`].
pub fn unpin(handle: NapiHandle, id: String) -> NapiResult<()> {
    ffi::unpin(RuntimeHandle(handle), id).map_err(NapiError::from)
}

/// Force-archive a memory record (user-initiated forget).
///
/// # Errors
///
/// Forwards [`ffi::forget`] errors as [`NapiError`].
pub fn forget(handle: NapiHandle, id: String) -> NapiResult<()> {
    ffi::forget(RuntimeHandle(handle), id).map_err(NapiError::from)
}

/// Destroy all cryptographic material for `scope_id` so its evidence
/// and body-table data become permanently unrecoverable. Mirrors
/// [`ffi::forget_scope`].
///
/// # Errors
///
/// Forwards [`ffi::forget_scope`] errors as [`NapiError`].
pub fn forget_scope(handle: NapiHandle, scope_id: ScopeIdString) -> NapiResult<()> {
    ffi::forget_scope(RuntimeHandle(handle), scope_id).map_err(NapiError::from)
}

/// Escape a user-supplied string for safe use inside an FTS5 query.
/// Mirrors [`ffi::escape_fts_query`].
pub fn escape_fts_query(input: String) -> String {
    ffi::escape_fts_query(input)
}

/// List memory records for a scope, optionally filtered.
///
/// # Errors
///
/// Forwards [`ffi::list_memories`] errors as [`NapiError`].
pub fn list_memories(
    handle: NapiHandle,
    scope_id: ScopeIdString,
    filter: MemoryFilter,
) -> NapiResult<Vec<MemoryRecord>> {
    ffi::list_memories(RuntimeHandle(handle), scope_id, filter).map_err(NapiError::from)
}

/// Fetch the channel-level synthesis memory for a scope.
///
/// # Errors
///
/// Forwards [`ffi::get_channel_memory`] errors as [`NapiError`].
pub fn get_channel_memory(
    handle: NapiHandle,
    scope_id: ScopeIdString,
) -> NapiResult<Option<MemoryRecord>> {
    ffi::get_channel_memory(RuntimeHandle(handle), scope_id).map_err(NapiError::from)
}

/// Run the per-scope memory decay sweep, demoting stale rows and
/// archiving anything that has aged out of the working set.
///
/// Mirrors [`ffi::run_decay_sweep`]. Returns the number of rows that
/// transitioned state during the sweep. Electron hosts call this on
/// idle ticks (typically every few minutes) to keep retention scores
/// fresh without blocking interactive paths.
///
/// # Errors
///
/// Forwards [`ffi::run_decay_sweep`] errors as [`NapiError`].
pub fn run_decay_sweep(handle: NapiHandle, scope_id: ScopeIdString) -> NapiResult<u32> {
    ffi::run_decay_sweep(RuntimeHandle(handle), scope_id).map_err(NapiError::from)
}

/// Trigger synthesis for a scope.
///
/// # Errors
///
/// Forwards [`ffi::trigger_synthesis`] errors as [`NapiError`].
pub fn trigger_synthesis(
    handle: NapiHandle,
    scope_id: ScopeIdString,
    trigger: SynthesisTrigger,
) -> NapiResult<String> {
    ffi::trigger_synthesis(RuntimeHandle(handle), scope_id, trigger).map_err(NapiError::from)
}

/// Generate a fresh signing keypair (post-quantum baseline).
///
/// # Errors
///
/// Forwards [`ffi::generate_keypair`] errors as [`NapiError`].
pub fn generate_keypair() -> NapiResult<FfiKeypair> {
    ffi::generate_keypair().map_err(NapiError::from)
}

/// Encrypt `plaintext` for `scope_id` using the scope-derived AEAD
/// key. Returns the ciphertext envelope as a base64 string suitable
/// for transport over JSON.
///
/// # Errors
///
/// Forwards [`ffi::encrypt`] errors as [`NapiError`].
#[allow(clippy::needless_pass_by_value)] // FFI: napi-derive hands owned strings across the JS boundary on every call; borrowing here would force an extra copy in generated code.
pub fn encrypt(
    handle: NapiHandle,
    scope_id: ScopeIdString,
    plaintext_b64: String,
) -> NapiResult<String> {
    let plaintext = decode_b64(&plaintext_b64)?;
    let cipher =
        ffi::encrypt(RuntimeHandle(handle), scope_id, plaintext).map_err(NapiError::from)?;
    Ok(encode_b64(&cipher))
}

/// Inverse of [`encrypt`].
///
/// # Errors
///
/// Forwards [`ffi::decrypt`] errors as [`NapiError`].
#[allow(clippy::needless_pass_by_value)] // FFI: napi-derive hands owned strings across the JS boundary on every call; borrowing here would force an extra copy in generated code.
pub fn decrypt(
    handle: NapiHandle,
    scope_id: ScopeIdString,
    ciphertext_b64: String,
) -> NapiResult<String> {
    let cipher = decode_b64(&ciphertext_b64)?;
    let plain = ffi::decrypt(RuntimeHandle(handle), scope_id, cipher).map_err(NapiError::from)?;
    Ok(encode_b64(&plain))
}

/// Return the semver of the Rust core baked into this build artefact.
///
/// Sourced from `CARGO_PKG_VERSION` at compile time, which mirrors
/// the workspace-level `version` in the root `Cargo.toml`. Hosts use
/// this to assert against a known-good core before opening any stores
/// so a stale addon from a previous install doesn't silently corrupt
/// data. The corresponding JS-facing wrapper is
/// [`bindings::js_core_version`].
pub fn core_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// Full health envelope sourced from the substrate's metrics +
/// tracing layer (Phase 6).
///
/// `handle` is optional:
/// * `None` (or [`RuntimeHandle::NONE`]) returns a bridge-only
///   envelope: just `core_version`, `uptime_secs`,
///   `tracing_initialized`, a single `bridge` subsystem, and a
///   metrics snapshot. Hosts call this immediately after loading the
///   addon, before any [`open_store`] call, to confirm the FFI layer
///   itself is reachable.
/// * `Some(handle)` for an open runtime returns a full envelope:
///   bridge + per-subsystem probes (`evidence_store`, `crypto`,
///   `memory_manager`, `inference_router`, `connector`). Each
///   subsystem is probed with real I/O —
///   * `evidence_store` runs a `SELECT COUNT(*)` against the open
///     SQLCipher connection;
///   * `crypto` verifies the master key is non-zero;
///   * `memory_manager` reports the rehydrated user / channel
///     memory counts;
///   * `inference_router` returns per-adapter availability via
///     [`inference_router::InferenceRouter::adapter_states`];
///   * `connector` reports total / authenticated / per-state
///     instance counts from the in-memory `connector_instances`
///     map and, when the `http-client` feature is enabled, also
///     surfaces whether the shared `BlockingHttpTransport` /
///     `OAuth2Client` finished initialising (degrading the
///     subsystem to `Degraded` with `http_transport=unavailable`
///     if `open_store` soft-failed transport construction). The
///     same envelope is what hosts use to detect the soft-fail
///     path without calling `js_create_connector` first.
///
/// Returns a [`ffi::HealthStatus`] which the napi binding wraps as
/// a `serde_json::Value` for the JS side.
///
/// # Why this is `NapiResult<…>` and not `HealthStatus`
///
/// The function's signature is `NapiResult<ffi::HealthStatus>`
/// because the `handle` argument has to be validated before any
/// probing can begin:
///
/// * **Bridge-only probe** — `handle = None` (or the `0n` sentinel)
///   skips the per-subsystem probes entirely and returns a
///   `HealthStatus` with just the `bridge` subsystem populated.
///   Always succeeds.
/// * **Full probe** — `handle = Some(h)` for a known-live handle
///   runs every subsystem probe and returns `Ok(HealthStatus)`
///   regardless of probe outcome: subsystem-level failures degrade
///   to `Degraded` / `Unavailable` entries inside the envelope, not
///   to a transport-level `Err`. The envelope must remain
///   returnable so hosts have a single inspectable surface for the
///   runtime's liveness.
/// * **Invalid handle** — `handle = Some(h)` for an unknown /
///   already-closed handle is the only path that surfaces as
///   `Err(NapiError::Ffi(FfiError::Unavailable { subsystem: "evidence_store" }))`.
///   This is intentional: hosts can distinguish "addon loaded but
///   runtime is closed / never opened" from "subsystem reports
///   degraded health" by inspecting whether the call returned an
///   `Err` or an `Ok` envelope with a `Degraded` entry.
pub fn health_check(handle: Option<NapiHandle>) -> NapiResult<ffi::HealthStatus> {
    // Treat the `NapiHandle::NONE` sentinel (`0n`) as "no handle"
    // so callers can pass `0n` interchangeably with `null`/`undefined`
    // from the JS side. Any other handle is forwarded as-is; an
    // unknown handle surfaces as `NapiError::Ffi(FfiError::Unavailable)`
    // so callers can distinguish "addon is loaded but runtime is
    // closed" from a bridge-only probe.
    let handle = handle.and_then(|h| {
        if h == RuntimeHandle::NONE.0 {
            None
        } else {
            Some(RuntimeHandle(h))
        }
    });
    ffi::health_check(handle).map_err(NapiError::from)
}

// ---------------------------------------------------------------------------
// Connector management — mirrors the six connector FFI functions defined in
// `crates/ffi/src/connector.rs`. The N-API wrappers in
// `crates/napi/src/bindings.rs` invoke these forwarders so the JS host gets
// the same lifecycle (`create` → `authenticate` → `sync` /
// `refresh` → `list` / `remove`) without going through the FFI surface twice.
// ---------------------------------------------------------------------------

/// Instantiate a connector for `kind`, bound to `scope_id`, with
/// `config_json` as the connector's `auth_config_json` payload.
/// Mirrors [`ffi::create_connector`].
///
/// # Errors
///
/// Forwards [`ffi::create_connector`] errors as [`NapiError`].
pub fn create_connector(
    handle: NapiHandle,
    kind: ConnectorKindTag,
    scope_id: ScopeIdString,
    config_json: String,
) -> NapiResult<String> {
    ffi::create_connector(RuntimeHandle(handle), kind, scope_id, config_json)
        .map_err(NapiError::from)
}

/// Run the OAuth2 `authorization_code` exchange for `instance_id`
/// and stash the bearer token in the per-runtime token vault.
/// Mirrors [`ffi::authenticate_connector`].
///
/// # Errors
///
/// Forwards [`ffi::authenticate_connector`] errors as [`NapiError`].
pub fn authenticate_connector(
    handle: NapiHandle,
    instance_id: String,
    auth_code: String,
) -> NapiResult<()> {
    ffi::authenticate_connector(RuntimeHandle(handle), instance_id, auth_code)
        .map_err(NapiError::from)
}

/// Run a sync against the source system and forward emitted
/// `ConnectorEvent`s into the encrypted evidence store. Mirrors
/// [`ffi::sync_connector`].
///
/// # Errors
///
/// Forwards [`ffi::sync_connector`] errors as [`NapiError`].
pub fn sync_connector(handle: NapiHandle, instance_id: String) -> NapiResult<SyncReport> {
    ffi::sync_connector(RuntimeHandle(handle), instance_id).map_err(NapiError::from)
}

/// List configured connector instances on this runtime with their
/// current sync state. Mirrors [`ffi::list_connectors`].
///
/// # Errors
///
/// Forwards [`ffi::list_connectors`] errors as [`NapiError`].
pub fn list_connectors(handle: NapiHandle) -> NapiResult<Vec<ConnectorStatus>> {
    ffi::list_connectors(RuntimeHandle(handle)).map_err(NapiError::from)
}

/// Tear down the connector with `instance_id`. Mirrors
/// [`ffi::remove_connector`].
///
/// # Errors
///
/// Forwards [`ffi::remove_connector`] errors as [`NapiError`].
pub fn remove_connector(handle: NapiHandle, instance_id: String) -> NapiResult<()> {
    ffi::remove_connector(RuntimeHandle(handle), instance_id).map_err(NapiError::from)
}

/// Drive an OAuth2 `grant_type=refresh_token` round-trip against
/// the provider's token endpoint, persist the refreshed token to
/// SQLCipher, and update the in-memory token vault. Mirrors
/// [`ffi::refresh_connector_token`].
///
/// # Errors
///
/// Forwards [`ffi::refresh_connector_token`] errors as
/// [`NapiError`]. The host should treat the `Connector` variant
/// carrying the framework's `TokenRefresh` diagnostic as
/// "re-authorisation required" and prompt the user through
/// [`authenticate_connector`] rather than retrying the refresh.
pub fn refresh_connector_token(
    handle: NapiHandle,
    instance_id: String,
) -> NapiResult<RefreshReport> {
    ffi::refresh_connector_token(RuntimeHandle(handle), instance_id).map_err(NapiError::from)
}

/// Register a host-supplied OAuth2 client-secret resolver on
/// `handle`'s per-runtime [`ffi::OAuth2Client`]. Mirrors
/// [`ffi::set_oauth_client_secret_resolver`].
///
/// `resolver` is an `Arc<dyn ffi::OAuthClientSecretResolver>` —
/// the N-API binding ([`bindings::js_set_oauth_client_secret_resolver`])
/// constructs this from a JS callback. Pure-Rust callers can pass
/// any `Arc<dyn OAuthClientSecretResolver>`.
///
/// # Errors
///
/// Forwards [`ffi::set_oauth_client_secret_resolver`] errors as
/// [`NapiError`].
pub fn set_oauth_client_secret_resolver(
    handle: NapiHandle,
    resolver: std::sync::Arc<dyn ffi::OAuthClientSecretResolver>,
) -> NapiResult<()> {
    ffi::set_oauth_client_secret_resolver(RuntimeHandle(handle), resolver).map_err(NapiError::from)
}

/// Unregister the previously-registered OAuth2 client-secret
/// resolver on `handle`. Mirrors
/// [`ffi::clear_oauth_client_secret_resolver`].
///
/// # Errors
///
/// Forwards [`ffi::clear_oauth_client_secret_resolver`] errors as
/// [`NapiError`].
pub fn clear_oauth_client_secret_resolver(handle: NapiHandle) -> NapiResult<()> {
    ffi::clear_oauth_client_secret_resolver(RuntimeHandle(handle)).map_err(NapiError::from)
}

// ───────────────────────── Webhook receiver (Phase 5) ─────────────

/// Start a webhook receiver server bound to `bind_addr` (parsed as
/// a `SocketAddr`). Mirrors [`ffi::start_webhook_server`].
///
/// `bind_addr` accepts IPv4 (`"127.0.0.1:9001"`), IPv6
/// (`"[::1]:9001"`), or `"0.0.0.0:0"` for an ephemeral port (the
/// resolved port is surfaced via [`list_webhook_servers`]).
///
/// # Errors
///
/// Forwards [`ffi::start_webhook_server`] errors as [`NapiError`].
pub fn start_webhook_server(
    handle: NapiHandle,
    bind_addr: String,
) -> NapiResult<ffi::WebhookServerHandle> {
    ffi::start_webhook_server(RuntimeHandle(handle), bind_addr).map_err(NapiError::from)
}

/// Stop a previously-started webhook server. Idempotent.
/// Mirrors [`ffi::stop_webhook_server`].
///
/// # Errors
///
/// Forwards [`ffi::stop_webhook_server`] errors as [`NapiError`].
pub fn stop_webhook_server(
    handle: NapiHandle,
    server_handle: ffi::WebhookServerHandle,
) -> NapiResult<()> {
    ffi::stop_webhook_server(RuntimeHandle(handle), server_handle).map_err(NapiError::from)
}

/// Bind `provider_id` to `instance_id` on `server_handle`.
/// Mirrors [`ffi::register_webhook_dispatch`].
///
/// # Errors
///
/// Forwards [`ffi::register_webhook_dispatch`] errors as [`NapiError`].
pub fn register_webhook_dispatch(
    handle: NapiHandle,
    server_handle: ffi::WebhookServerHandle,
    provider_id: String,
    instance_id: String,
) -> NapiResult<()> {
    ffi::register_webhook_dispatch(
        RuntimeHandle(handle),
        server_handle,
        provider_id,
        instance_id,
    )
    .map_err(NapiError::from)
}

/// Drop the binding for `(server_handle, provider_id)`. Idempotent.
/// Mirrors [`ffi::unregister_webhook_dispatch`].
///
/// # Errors
///
/// Forwards [`ffi::unregister_webhook_dispatch`] errors as [`NapiError`].
pub fn unregister_webhook_dispatch(
    handle: NapiHandle,
    server_handle: ffi::WebhookServerHandle,
    provider_id: String,
) -> NapiResult<()> {
    ffi::unregister_webhook_dispatch(RuntimeHandle(handle), server_handle, provider_id)
        .map_err(NapiError::from)
}

/// Enumerate every running webhook server on `handle` with its
/// per-server counters. Mirrors [`ffi::list_webhook_servers`].
///
/// # Errors
///
/// Forwards [`ffi::list_webhook_servers`] errors as [`NapiError`].
pub fn list_webhook_servers(handle: NapiHandle) -> NapiResult<Vec<ffi::WebhookServerSummary>> {
    ffi::list_webhook_servers(RuntimeHandle(handle)).map_err(NapiError::from)
}

/// Start the background sync scheduler (Phase 6).
///
/// Spawns a dedicated OS thread that wakes every
/// `tick_interval_secs` and dispatches [`ffi::sync_connector`] for
/// every connector instance whose `last_synced_at + sync_interval`
/// has elapsed. Mirrors [`ffi::start_sync_scheduler`].
///
/// # Errors
///
/// Forwards [`ffi::start_sync_scheduler`] errors as [`NapiError`].
pub fn start_sync_scheduler(
    handle: NapiHandle,
    default_interval_secs: u64,
    default_max_backoff_secs: u64,
    tick_interval_secs: u64,
) -> NapiResult<()> {
    ffi::start_sync_scheduler(
        RuntimeHandle(handle),
        default_interval_secs,
        default_max_backoff_secs,
        tick_interval_secs,
    )
    .map_err(NapiError::from)
}

/// Stop the background sync scheduler (Phase 6).
///
/// Idempotent — calling this on a runtime with no scheduler running
/// returns `Ok(())`. Mirrors [`ffi::stop_sync_scheduler`].
///
/// # Errors
///
/// Forwards [`ffi::stop_sync_scheduler`] errors as [`NapiError`].
pub fn stop_sync_scheduler(handle: NapiHandle) -> NapiResult<()> {
    ffi::stop_sync_scheduler(RuntimeHandle(handle)).map_err(NapiError::from)
}

/// Override the scheduler's policy for a specific connector
/// instance (Phase 6). Mirrors [`ffi::configure_sync_schedule`].
///
/// # Errors
///
/// Forwards [`ffi::configure_sync_schedule`] errors as [`NapiError`].
pub fn configure_sync_schedule(
    handle: NapiHandle,
    instance_id: String,
    sync_interval_secs: u64,
    max_backoff_secs: u64,
) -> NapiResult<()> {
    ffi::configure_sync_schedule(
        RuntimeHandle(handle),
        instance_id,
        sync_interval_secs,
        max_backoff_secs,
    )
    .map_err(NapiError::from)
}

/// Remove the per-instance scheduler policy override for
/// `instance_id` (Phase 6). Mirrors [`ffi::clear_sync_schedule`].
///
/// # Errors
///
/// Forwards [`ffi::clear_sync_schedule`] errors as [`NapiError`].
pub fn clear_sync_schedule(handle: NapiHandle, instance_id: String) -> NapiResult<()> {
    ffi::clear_sync_schedule(RuntimeHandle(handle), instance_id).map_err(NapiError::from)
}

/// Snapshot the scheduler's diagnostic state (Phase 6). Mirrors
/// [`ffi::sync_scheduler_status`].
///
/// # Errors
///
/// Forwards [`ffi::sync_scheduler_status`] errors as [`NapiError`].
pub fn sync_scheduler_status(handle: NapiHandle) -> NapiResult<ffi::SyncSchedulerStatus> {
    ffi::sync_scheduler_status(RuntimeHandle(handle)).map_err(NapiError::from)
}

/// Toggle the post-sync auto-synthesis hook for a connector
/// instance (Phase 7). Mirrors [`ffi::configure_sync_auto_synthesize`].
///
/// # Errors
///
/// Forwards [`ffi::configure_sync_auto_synthesize`] errors as
/// [`NapiError`].
pub fn configure_sync_auto_synthesize(
    handle: NapiHandle,
    instance_id: String,
    enabled: bool,
) -> NapiResult<()> {
    ffi::configure_sync_auto_synthesize(RuntimeHandle(handle), instance_id, enabled)
        .map_err(NapiError::from)
}

/// Install the server-side synthesis engine on the runtime
/// (Phase 7). Mirrors [`ffi::configure_synthesis_engine`].
///
/// # Errors
///
/// Forwards [`ffi::configure_synthesis_engine`] errors as
/// [`NapiError`].
#[allow(clippy::needless_pass_by_value)] // FFI: napi-derive hands owned values across the JS boundary on every call.
pub fn configure_synthesis_engine(
    handle: NapiHandle,
    config: ffi::SynthesisEngineConfig,
) -> NapiResult<()> {
    ffi::configure_synthesis_engine(RuntimeHandle(handle), config).map_err(NapiError::from)
}

/// Dispatch a server-side synthesis run (Phase 7). Mirrors
/// [`ffi::trigger_server_synthesis`]. Returns the UUID of the
/// newly-opened synthesis window.
///
/// # Errors
///
/// Forwards [`ffi::trigger_server_synthesis`] errors as
/// [`NapiError`].
pub fn trigger_server_synthesis(
    handle: NapiHandle,
    scope_id: ScopeIdString,
    tier: ffi::SynthesisTierKind,
) -> NapiResult<String> {
    ffi::trigger_server_synthesis(RuntimeHandle(handle), scope_id, tier).map_err(NapiError::from)
}

/// Look up the lifecycle state of a synthesis window (Phase 7).
/// Mirrors [`ffi::synthesis_status`].
///
/// # Errors
///
/// Forwards [`ffi::synthesis_status`] errors as [`NapiError`].
pub fn synthesis_status(
    handle: NapiHandle,
    synthesis_id: String,
) -> NapiResult<ffi::SynthesisStatusRecord> {
    ffi::synthesis_status(RuntimeHandle(handle), synthesis_id).map_err(NapiError::from)
}

/// Enumerate recent synthesis windows for a scope (Phase 7).
/// Mirrors [`ffi::list_recent_syntheses`].
///
/// # Errors
///
/// Forwards [`ffi::list_recent_syntheses`] errors as [`NapiError`].
pub fn list_recent_syntheses(
    handle: NapiHandle,
    scope_id: ScopeIdString,
) -> NapiResult<Vec<ffi::SynthesisStatusRecord>> {
    ffi::list_recent_syntheses(RuntimeHandle(handle), scope_id).map_err(NapiError::from)
}

/// Admit an approved document onto a tenant memory and persist the
/// AEAD-encrypted payload alongside (Phase 8). Mirrors
/// [`ffi::admit_approved_document`].
///
/// # Errors
///
/// Forwards [`ffi::admit_approved_document`] errors as [`NapiError`].
pub fn admit_approved_document(
    handle: NapiHandle,
    scope_id: ScopeIdString,
    label: String,
    approver: String,
    payload: Vec<u8>,
) -> NapiResult<ffi::ApprovedDocumentSummary> {
    ffi::admit_approved_document(RuntimeHandle(handle), scope_id, label, approver, payload)
        .map_err(NapiError::from)
}

/// Replace the payload + metadata of an existing approved document
/// while keeping its document id stable (Phase 9). Mirrors
/// [`ffi::replace_approved_document`].
///
/// # Errors
///
/// Forwards [`ffi::replace_approved_document`] errors as [`NapiError`].
pub fn replace_approved_document(
    handle: NapiHandle,
    scope_id: ScopeIdString,
    document_id: String,
    label: String,
    approver: String,
    payload: Vec<u8>,
) -> NapiResult<ffi::ApprovedDocumentSummary> {
    ffi::replace_approved_document(
        RuntimeHandle(handle),
        scope_id,
        document_id,
        label,
        approver,
        payload,
    )
    .map_err(NapiError::from)
}

/// Revoke a previously-admitted approved document and purge the
/// AEAD-encrypted payload row (Phase 8). Mirrors
/// [`ffi::revoke_approved_document`].
///
/// # Errors
///
/// Forwards [`ffi::revoke_approved_document`] errors as [`NapiError`].
pub fn revoke_approved_document(
    handle: NapiHandle,
    scope_id: ScopeIdString,
    document_id: String,
) -> NapiResult<()> {
    ffi::revoke_approved_document(RuntimeHandle(handle), scope_id, document_id)
        .map_err(NapiError::from)
}

/// List approved documents admitted onto a tenant memory along
/// with their persisted payload metadata (Phase 8). Mirrors
/// [`ffi::list_approved_documents`].
///
/// # Errors
///
/// Forwards [`ffi::list_approved_documents`] errors as [`NapiError`].
pub fn list_approved_documents(
    handle: NapiHandle,
    scope_id: ScopeIdString,
) -> NapiResult<Vec<ffi::ApprovedDocumentSummary>> {
    ffi::list_approved_documents(RuntimeHandle(handle), scope_id).map_err(NapiError::from)
}

const B64_ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn encode_b64(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        let v = (u32::from(b0) << 16) | (u32::from(b1) << 8) | u32::from(b2);
        out.push(B64_ALPHABET[((v >> 18) & 0x3F) as usize] as char);
        out.push(B64_ALPHABET[((v >> 12) & 0x3F) as usize] as char);
        out.push(if chunk.len() > 1 {
            B64_ALPHABET[((v >> 6) & 0x3F) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            B64_ALPHABET[(v & 0x3F) as usize] as char
        } else {
            '='
        });
    }
    out
}

fn decode_b64(s: &str) -> NapiResult<Vec<u8>> {
    let s = s.as_bytes();
    if !s.len().is_multiple_of(4) {
        return Err(NapiError::InvalidArgument {
            message: "base64 input length must be a multiple of 4".into(),
        });
    }
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    for chunk in s.chunks(4) {
        let mut v = 0u32;
        let mut pad = 0;
        for &c in chunk {
            v <<= 6;
            if c == b'=' {
                pad += 1;
                continue;
            }
            let idx =
                B64_ALPHABET
                    .iter()
                    .position(|&x| x == c)
                    .ok_or(NapiError::InvalidArgument {
                        message: "invalid base64 character".into(),
                    })?;
            // `idx` is bounded by `B64_ALPHABET.len() == 64`, so the
            // conversion is lossless. `try_from` keeps the cast lints
            // happy without resorting to `as`.
            v |= u32::try_from(idx).expect("base64 alphabet index always fits in u32");
        }
        // Each byte extracted is masked to 0xFF before the cast, so
        // truncation is the intended semantic.
        #[allow(clippy::cast_possible_truncation)]
        {
            out.push(((v >> 16) & 0xFF) as u8);
            if pad < 2 {
                out.push(((v >> 8) & 0xFF) as u8);
            }
            if pad < 1 {
                out.push((v & 0xFF) as u8);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_accepts_valid_config() {
        let cfg = InitConfig {
            data_dir: "/tmp/knowledge".into(),
            log_level: "info".into(),
        };
        let json = serde_json::to_string(&cfg).unwrap();
        init(&json).unwrap();
    }

    #[test]
    fn init_rejects_invalid_json() {
        let err = init("not-json").unwrap_err();
        assert!(matches!(err, NapiError::InvalidConfig { .. }));
    }

    #[test]
    fn ingest_request_round_trips() {
        let req = IngestRequest {
            scope_id: "scope".into(),
            body: "hi".into(),
            source: SourceKind::Manual,
            importance: FfiImportanceClass::Important,
        };
        let s = serde_json::to_string(&req).unwrap();
        let back: IngestRequest = serde_json::from_str(&s).unwrap();
        assert_eq!(req, back);

        // Importance field defaults to Important when absent from JSON.
        let minimal = r#"{"scope_id":"s","body":"b","source":"Manual"}"#;
        let parsed: IngestRequest = serde_json::from_str(minimal).unwrap();
        assert_eq!(parsed.importance, FfiImportanceClass::Important);
    }

    #[test]
    fn ingest_message_forwards_invalid_id_for_malformed_scope() {
        // The FFI surface parses `scope_id` as a UUID. Hosts that
        // forget to validate the JS-side string should get a
        // structured `InvalidId` back rather than a panic.
        let req = IngestRequest {
            scope_id: "scope".into(),
            body: "hi".into(),
            source: SourceKind::Slack,
            importance: FfiImportanceClass::Important,
        };
        let err = ingest_message(RuntimeHandle::NONE.0, req).unwrap_err();
        assert_eq!(err.kind(), "InvalidId");
    }

    #[test]
    fn query_request_round_trips() {
        let req = QueryRequest {
            scope_id: "scope".into(),
            query_text: "q".into(),
            limit: 10,
        };
        let s = serde_json::to_string(&req).unwrap();
        let back: QueryRequest = serde_json::from_str(&s).unwrap();
        assert_eq!(req, back);
    }

    #[test]
    fn query_forwards_invalid_id_for_malformed_scope() {
        let req = QueryRequest {
            scope_id: "scope".into(),
            query_text: "q".into(),
            limit: 10,
        };
        let err = query(RuntimeHandle::NONE.0, req).unwrap_err();
        assert_eq!(err.kind(), "InvalidId");
    }

    #[test]
    fn pin_unpin_forward_invalid_id_for_malformed_id() {
        // `pin` / `unpin` validate the id as a UUID before walking
        // the memory layer, so malformed strings surface as
        // structured `InvalidId` rather than panicking through the
        // FFI bridge.
        for f in [
            pin as fn(NapiHandle, String) -> NapiResult<()>,
            unpin as fn(NapiHandle, String) -> NapiResult<()>,
        ] {
            let err = f(RuntimeHandle::NONE.0, "id".into()).unwrap_err();
            assert_eq!(err.kind(), "InvalidId");
        }
    }

    #[test]
    fn forget_forwards_invalid_id_for_malformed_id() {
        // `forget` is wired: it validates the id is a
        // UUID before touching the runtime, so malformed ids surface
        // as `InvalidId`.
        let err = forget(RuntimeHandle::NONE.0, "id".into()).unwrap_err();
        assert_eq!(err.kind(), "InvalidId");
    }

    #[test]
    fn forget_scope_forwards_invalid_id_for_malformed_scope() {
        let err = forget_scope(RuntimeHandle::NONE.0, "not-a-uuid".into()).unwrap_err();
        assert_eq!(err.kind(), "InvalidId");
    }

    #[test]
    fn escape_fts_query_wraps_in_quotes() {
        let escaped = escape_fts_query(r#"hello "world""#.into());
        assert_eq!(escaped, r#""hello ""world""""#);
    }

    #[test]
    fn list_memories_forwards_invalid_id_for_malformed_scope() {
        // `list_memories` is wired — the surface
        // validates the scope id is a UUID before reaching the
        // memory layer.
        let err = list_memories(
            RuntimeHandle::NONE.0,
            "scope".into(),
            MemoryFilter {
                state: Some(MemoryState::Reinforced),
                pinned_only: false,
            },
        )
        .unwrap_err();
        assert_eq!(err.kind(), "InvalidId");
    }

    #[test]
    fn synthesis_endpoints_forward_invalid_id_for_malformed_scope() {
        // `get_channel_memory` is wired; `trigger_synthesis` parses
        // the scope id before returning the `Unavailable`
        // marker. Both should report InvalidId for a malformed id.
        assert_eq!(
            get_channel_memory(RuntimeHandle::NONE.0, "scope".into())
                .unwrap_err()
                .kind(),
            "InvalidId"
        );
        assert_eq!(
            trigger_synthesis(
                RuntimeHandle::NONE.0,
                "scope".into(),
                SynthesisTrigger::ManualUserAction
            )
            .unwrap_err()
            .kind(),
            "InvalidId"
        );
    }

    #[test]
    fn run_decay_sweep_forwards_invalid_id_for_malformed_scope() {
        // Mirrors the FFI-side run_decay_sweep contract: a malformed
        // scope id is rejected before the runtime is touched, so this
        // surfaces InvalidId rather than Unavailable.
        let err = run_decay_sweep(RuntimeHandle::NONE.0, "scope".into()).unwrap_err();
        assert_eq!(err.kind(), "InvalidId");
    }

    #[test]
    fn generate_keypair_returns_ml_dsa_65_envelope() {
        // Wired. The N-API layer just forwards the
        // structured envelope; assert the envelope shape is
        // preserved across the bridge.
        let kp = generate_keypair().expect("generate_keypair");
        assert_eq!(kp.algorithm, "ml-dsa-65");
        assert!(!kp.public_key.is_empty());
        assert!(!kp.private_key.is_empty());
    }

    #[test]
    fn encrypt_decrypt_forward_invalid_id_for_malformed_scope() {
        // The N-API layer base64-decodes the payload and forwards to
        // FFI. With a malformed scope string FFI rejects with
        // InvalidId before any crypto work happens.
        let err = encrypt(
            RuntimeHandle::NONE.0,
            "scope".into(),
            encode_b64(&[1, 2, 3]),
        )
        .unwrap_err();
        assert_eq!(err.kind(), "InvalidId");
        let err = decrypt(
            RuntimeHandle::NONE.0,
            "scope".into(),
            encode_b64(&[1, 2, 3]),
        )
        .unwrap_err();
        assert_eq!(err.kind(), "InvalidId");
    }

    #[test]
    fn b64_codec_round_trips() {
        let inputs: &[&[u8]] = &[
            &[],
            &[0x00],
            &[0x00, 0x01],
            &[0x00, 0x01, 0x02],
            &[0x00, 0x01, 0x02, 0x03],
            b"hello world",
        ];
        for input in inputs {
            let s = encode_b64(input);
            let back = decode_b64(&s).unwrap();
            assert_eq!(*input, &back[..]);
        }
    }

    #[test]
    fn b64_decode_rejects_invalid_input() {
        assert!(decode_b64("AAA").is_err()); // wrong length
        assert!(decode_b64("AA!=").is_err()); // invalid char
    }
}
