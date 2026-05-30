//! N-API bindings entry point.
//!
//! This module exposes the substrate's desktop bridge as a Node.js
//! native addon via `napi-rs` [`#[napi]`] proc-macros. The pure-Rust
//! surface in [`crate`] stays the canonical Rust-facing API; every
//! `#[napi]`-annotated wrapper here is a thin adapter that:
//!
//! 1. Converts JS-side argument types (`BigInt`, plain JS objects)
//!    into the Rust-side types ([`NapiHandle`] is `u64`, complex
//!    requests come in as [`serde_json::Value`] and are
//!    `from_value`-parsed into the typed [`IngestRequest`] /
//!    [`QueryRequest`] / [`MemoryFilter`] / [`SynthesisTrigger`]).
//! 2. Forwards the call to the matching pure-Rust function.
//! 3. Maps any [`NapiError`] into a structured [`napi::Error`] whose
//!    `reason` is a JSON envelope `{"kind": "...", "detail": {...}}`
//!    so JavaScript callers can switch on `JSON.parse(e.message).kind`.
//!
//! The bindings are compiled in the `cdylib` artefact picked up by
//! `napi build`. They are also reachable from Rust (via the `rlib`
//! crate-type) so the unit tests in this file can exercise the same
//! JSON-envelope error path that JavaScript callers see at runtime.
//!
//! ## Naming
//!
//! `napi-derive` automatically renames Rust `snake_case` identifiers
//! to JS `camelCase` ones when generating the JS surface (e.g.
//! `open_store` → `openStore`). This is the documented behaviour;
//! see <https://napi.rs/docs/concepts/values#naming>.

#![allow(clippy::needless_pass_by_value)]
// napi-derive hands owned values across the JS boundary every call; borrowing would force an extra copy in generated code.
#![allow(unsafe_code)] // napi-derive's `#[napi]` proc-macro expands into FFI module-init stubs that necessarily touch raw C pointers (napi_env, napi_callback_info, napi_value). The expansion includes its own `#[allow(unsafe_code)]` on every generated `extern "C"` function; for the workspace-level `deny(unsafe_code)` to be overridable we mirror the allow here. The hand-written code in this module remains `unsafe`-free.

use napi::bindgen_prelude::{BigInt, Error as JsError, Function, Object, Result};
use napi::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};
use napi_derive::napi;

#[cfg(test)]
use ffi::RuntimeHandle;
use ffi::{
    ConnectorHealthRecord, ConnectorKindTag, ConnectorStatus, FfiError, MemoryFilter, MemoryRecord,
    RefreshReport, SyncReport, SynthesisTrigger,
};

use crate::types::{IngestRequest, QueryRequest};
use crate::{NapiError, NapiHandle};

/// Convert a [`NapiError`] into a structured [`napi::Error`].
///
/// The JS-side `Error.message` is a JSON string of the form
/// `{"kind":"InvalidId","message":"...","detail":{...}}` where:
///
/// * `kind` is the flattened, finest-grained kind tag — for a
///   wrapped `Ffi(InvalidId)` it is `"InvalidId"`, not `"Ffi"`. This
///   is what JS callers switch on.
/// * `message` is the human-readable [`Display`] string suitable
///   for surfacing to the UI.
/// * `detail` is the full serialized [`NapiError`] (including the
///   outer `Ffi` wrapper) for callers that need the raw structure
///   for telemetry / logging.
///
/// Callers do:
///
/// ```js
/// try { core.openStore(...) }
/// catch (e) {
///   const env = JSON.parse(e.message);
///   if (env.kind === "InvalidId") { /* surface as user-input bug */ }
/// }
/// ```
///
/// We deliberately do NOT map kinds to napi's fixed `Status` enum
/// (e.g. `InvalidArg`) because that erases the finer-grained kind
/// surface (`InvalidId` vs `InvalidArgument` would both become
/// `InvalidArg`). The JSON envelope preserves full fidelity.
fn to_js_error(err: NapiError) -> JsError {
    let envelope = serde_json::json!({
        "kind": err.kind(),
        "message": err.to_string(),
        "detail": serde_json::to_value(&err).unwrap_or(serde_json::Value::Null),
    });
    JsError::from_reason(envelope.to_string())
}

/// Convert a JS `BigInt` handle (as marshalled by `napi-derive`) into
/// the substrate's [`NapiHandle`].
///
/// JS represents [`NapiHandle`] as a `BigInt` so the full 64-bit
/// width round-trips without precision loss (JS `Number` only has 53
/// bits of mantissa). napi-rs' `BigInt::get_u64()` returns
/// `(sign, value, lossless)` — we reject negative or too-large
/// BigInts so opaque host bugs can't smuggle a corrupted handle into
/// the runtime.
fn handle_from_bigint(handle: &BigInt) -> Result<NapiHandle> {
    let (sign, value, lossless) = handle.get_u64();
    if sign {
        return Err(to_js_error(NapiError::InvalidArgument {
            message: "handle must be a non-negative BigInt".into(),
        }));
    }
    if !lossless {
        return Err(to_js_error(NapiError::InvalidArgument {
            message: "handle does not fit in a 64-bit unsigned integer".into(),
        }));
    }
    Ok(value)
}

/// Parse a JSON-shaped argument into a typed Rust struct.
///
/// The argument arrives from JS as a plain `Object` which napi-rs
/// converts to [`serde_json::Value`] thanks to the `serde-json`
/// feature on the `napi` crate. From there, `serde_json::from_value`
/// produces the typed request struct used by the pure-Rust API.
fn parse_arg<T: serde::de::DeserializeOwned>(value: serde_json::Value) -> Result<T> {
    serde_json::from_value(value).map_err(|e| {
        to_js_error(NapiError::InvalidArgument {
            message: format!("invalid request payload: {e}"),
        })
    })
}

// ---------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------

/// Initialise the Rust core. Mirrors [`crate::init`].
#[napi(js_name = "init")]
pub fn js_init(config_json: String) -> Result<()> {
    crate::init(&config_json).map_err(to_js_error)
}

/// Open the SQLCipher-backed evidence store. Mirrors
/// [`crate::open_store`]. Returns a JS `BigInt` handle the caller
/// passes back into every subsequent call.
#[napi(js_name = "openStore")]
pub fn js_open_store(path: String, master_key_hex: String) -> Result<BigInt> {
    let handle = crate::open_store(path, master_key_hex).map_err(to_js_error)?;
    Ok(BigInt::from(handle))
}

/// Drop the open evidence store. Mirrors [`crate::close_store`].
#[napi(js_name = "closeStore")]
pub fn js_close_store(handle: BigInt) -> Result<()> {
    let h = handle_from_bigint(&handle)?;
    crate::close_store(h).map_err(to_js_error)
}

// ---------------------------------------------------------------------------
// Evidence plane
// ---------------------------------------------------------------------------

/// Ingest a chat / document message. Mirrors
/// [`crate::ingest_message`]. The `req` argument is a plain JS
/// object shaped like [`IngestRequest`].
#[napi(js_name = "ingestMessage")]
pub fn js_ingest_message(handle: BigInt, req: serde_json::Value) -> Result<serde_json::Value> {
    let h = handle_from_bigint(&handle)?;
    let typed: IngestRequest = parse_arg(req)?;
    crate::ingest_message(h, typed).map_err(to_js_error)
}

/// Hybrid query against a scope. Mirrors [`crate::query`].
#[napi(js_name = "query")]
pub fn js_query(handle: BigInt, req: serde_json::Value) -> Result<serde_json::Value> {
    let h = handle_from_bigint(&handle)?;
    let typed: QueryRequest = parse_arg(req)?;
    let rows = crate::query(h, typed).map_err(to_js_error)?;
    serde_json::to_value(rows).map_err(|e| {
        // `Internal`, not `InvalidArgument`: the JS caller's input
        // has already cleared `parse_arg` and the underlying
        // `crate::query` call has already returned successfully.
        // A `to_value` failure here means the substrate has a
        // latent encoding bug on a `Serialize` impl that should
        // be infallible by construction — routing this to the
        // `Internal` bucket keeps caller-error telemetry clean.
        to_js_error(NapiError::Internal {
            message: format!("failed to serialise query rows: {e}"),
        })
    })
}

/// Fetch a single evidence row. Mirrors [`crate::get_evidence`].
#[napi(js_name = "getEvidence")]
pub fn js_get_evidence(handle: BigInt, evidence_id: String) -> Result<serde_json::Value> {
    let h = handle_from_bigint(&handle)?;
    let row = crate::get_evidence(h, evidence_id).map_err(to_js_error)?;
    serde_json::to_value(row).map_err(|e| {
        // See `js_query` — a substrate-side encoding bug, not
        // a caller-side input bug.
        to_js_error(NapiError::Internal {
            message: format!("failed to serialise evidence row: {e}"),
        })
    })
}

/// Escape a user-supplied FTS5 query. Mirrors
/// [`crate::escape_fts_query`].
#[napi(js_name = "escapeFtsQuery")]
pub fn js_escape_fts_query(input: String) -> String {
    crate::escape_fts_query(input)
}

// ---------------------------------------------------------------------------
// Memory plane
// ---------------------------------------------------------------------------

/// Fetch the per-user memory bundle for a scope. Mirrors
/// [`crate::get_user_memory`].
#[napi(js_name = "getUserMemory")]
pub fn js_get_user_memory(handle: BigInt, scope_id: String) -> Result<serde_json::Value> {
    let h = handle_from_bigint(&handle)?;
    let rows: Vec<MemoryRecord> = crate::get_user_memory(h, scope_id).map_err(to_js_error)?;
    serde_json::to_value(rows).map_err(|e| {
        // See `js_query` — a substrate-side encoding bug, not
        // a caller-side input bug.
        to_js_error(NapiError::Internal {
            message: format!("failed to serialise memory rows: {e}"),
        })
    })
}

/// Pin a memory record. Mirrors [`crate::pin`].
#[napi(js_name = "pin")]
pub fn js_pin(handle: BigInt, id: String) -> Result<()> {
    let h = handle_from_bigint(&handle)?;
    crate::pin(h, id).map_err(to_js_error)
}

/// Lift a pin. Mirrors [`crate::unpin`].
#[napi(js_name = "unpin")]
pub fn js_unpin(handle: BigInt, id: String) -> Result<()> {
    let h = handle_from_bigint(&handle)?;
    crate::unpin(h, id).map_err(to_js_error)
}

/// Force-archive a memory record. Mirrors [`crate::forget`].
#[napi(js_name = "forget")]
pub fn js_forget(handle: BigInt, id: String) -> Result<()> {
    let h = handle_from_bigint(&handle)?;
    crate::forget(h, id).map_err(to_js_error)
}

/// Destroy all key material for a scope. Mirrors
/// [`crate::forget_scope`].
#[napi(js_name = "forgetScope")]
pub fn js_forget_scope(handle: BigInt, scope_id: String) -> Result<()> {
    let h = handle_from_bigint(&handle)?;
    crate::forget_scope(h, scope_id).map_err(to_js_error)
}

/// List memory records, optionally filtered. Mirrors
/// [`crate::list_memories`]. The `filter` argument is a JSON object
/// whose key set must match [`MemoryFilter`] exactly — `state` is
/// optional (`null` or omitted ⇒ no state filter) and `pinned_only`
/// is a required bool. The struct carries
/// `#[serde(deny_unknown_fields)]`, so a typo like `pinnedOnly`
/// (camelCase) or `Pinned_Only` errors out with a clear
/// `InvalidArgument` rather than silently defaulting — this catches
/// JS-side mistakes at the FFI boundary instead of letting them
/// surface as missing memory rows later in the pipeline.
#[napi(js_name = "listMemories")]
pub fn js_list_memories(
    handle: BigInt,
    scope_id: String,
    filter: serde_json::Value,
) -> Result<serde_json::Value> {
    let h = handle_from_bigint(&handle)?;
    let typed: MemoryFilter = parse_arg(filter)?;
    let rows: Vec<MemoryRecord> = crate::list_memories(h, scope_id, typed).map_err(to_js_error)?;
    serde_json::to_value(rows).map_err(|e| {
        // See `js_query` — a substrate-side encoding bug, not
        // a caller-side input bug.
        to_js_error(NapiError::Internal {
            message: format!("failed to serialise memory rows: {e}"),
        })
    })
}

/// Run the per-scope memory decay sweep. Mirrors
/// [`crate::run_decay_sweep`].
#[napi(js_name = "runDecaySweep")]
pub fn js_run_decay_sweep(handle: BigInt, scope_id: String) -> Result<u32> {
    let h = handle_from_bigint(&handle)?;
    crate::run_decay_sweep(h, scope_id).map_err(to_js_error)
}

// ---------------------------------------------------------------------------
// Synthesis plane
// ---------------------------------------------------------------------------

/// Fetch the channel-level synthesis memory. Mirrors
/// [`crate::get_channel_memory`].
#[napi(js_name = "getChannelMemory")]
pub fn js_get_channel_memory(handle: BigInt, scope_id: String) -> Result<serde_json::Value> {
    let h = handle_from_bigint(&handle)?;
    let row: Option<MemoryRecord> = crate::get_channel_memory(h, scope_id).map_err(to_js_error)?;
    serde_json::to_value(row).map_err(|e| {
        // See `js_query` — a substrate-side encoding bug, not
        // a caller-side input bug.
        to_js_error(NapiError::Internal {
            message: format!("failed to serialise channel memory: {e}"),
        })
    })
}

/// Trigger synthesis for a scope. Mirrors
/// [`crate::trigger_synthesis`]. `trigger` is one of
/// `"ManualUserAction"` / `"BackgroundIdle"` / `"EvidenceThreshold"` /
/// `"ConnectorSyncCompleted"` (matching the
/// [`SynthesisTrigger`] enum's serde-rename rules).
#[napi(js_name = "triggerSynthesis")]
pub fn js_trigger_synthesis(handle: BigInt, scope_id: String, trigger: String) -> Result<String> {
    let h = handle_from_bigint(&handle)?;
    let trig: SynthesisTrigger = parse_arg(serde_json::Value::String(trigger))?;
    crate::trigger_synthesis(h, scope_id, trig).map_err(to_js_error)
}

// ---------------------------------------------------------------------------
// Server-side synthesis (Phase 7).
// ---------------------------------------------------------------------------

/// Install the server-side synthesis engine on the runtime.
/// Mirrors [`crate::configure_synthesis_engine`].
///
/// `config` is the JSON object documented on
/// [`ffi::SynthesisEngineConfig`] with camelCase keys:
/// `{ url, apiKeyRef, modelId, maxTokens, timeoutMs, grammar,
///    scopeBindings, singleTenant, rateCapacity, rateRefillPerSec }`.
///
/// * `scopeBindings`, if present, is an array of UUID strings the
///   FFI layer admits for dispatch (production multi-tenant
///   deployments SHOULD configure it; an absent allow-list logs a
///   warning on every dispatch).
/// * `singleTenant` (defaults to `false`) is a host-supplied
///   posture flag. When `true`, the health probe reports
///   `Nominal` instead of `Degraded` if the engine is configured
///   without `scopeBindings` — appropriate for dev / single-tenant
///   deployments where there is no cross-scope allow-list to
///   enforce. Multi-tenant production deployments should leave
///   this `false` (the default) and provide `scopeBindings`.
/// * `rateCapacity` (Phase 10 Item 5) is the burst capacity of
///   the global token-bucket rate limiter on
///   `triggerServerSynthesis`. `0` (the default if the key is
///   omitted) falls back to
///   [`ffi::synthesis::DEFAULT_TRIGGER_RATE_CAPACITY`] (`8`).
/// * `rateRefillPerSec` (Phase 10 Item 5) is the token refill
///   rate in tokens/second. `0.0` falls back to
///   [`ffi::synthesis::DEFAULT_TRIGGER_RATE_REFILL_PER_SEC`]
///   (`1.0`). Fractional values are supported; non-finite or
///   negative values are rejected with `Unavailable`.
///
/// # Errors
///
/// * `Unavailable` if `openStore(handle)` has not been called,
///   the build lacks the `http-client` feature, or
///   `rateRefillPerSec` is non-finite / non-positive.
/// * `InvalidArgument` if `config.url` is empty or any
///   `scopeBindings` entry fails to parse as a UUID.
#[napi(js_name = "configureSynthesisEngine")]
pub fn js_configure_synthesis_engine(handle: BigInt, config: serde_json::Value) -> Result<()> {
    let h = handle_from_bigint(&handle)?;
    let typed: ffi::SynthesisEngineConfig = parse_arg(config)?;
    crate::configure_synthesis_engine(h, typed).map_err(to_js_error)
}

/// Dispatch a server-side synthesis run for `scopeId` at the
/// requested `tier`. Mirrors [`crate::trigger_server_synthesis`].
/// `tier` is `"domain"` or `"tenant"` (matches
/// [`ffi::SynthesisTierKind`]'s serde rename rules).
///
/// Returns the UUID string of the newly-opened synthesis window.
/// Callers can poll [`js_synthesis_status`] with the same value
/// to observe state transitions.
///
/// # Errors
///
/// * `Unavailable` if no engine is configured or the runtime has
///   been forgotten for this scope.
/// * `NotFound` if the scope has no domain / tenant memory
///   object registered.
/// * `Synthesis` for engine, validation, or persistence
///   failures.
/// * `InvalidArgument` if `scopeId` is not a UUID or `tier` is
///   not one of the documented values.
/// * `Throttled` (Phase 10 Item 5) if the global token-bucket
///   rate limiter rejects the call. The error carries a
///   `retryAfterMs` field — the host SHOULD wait that long and
///   retry the same call rather than treating this as a
///   permanent failure. Tune the limiter via `configureSynthesisEngine`'s
///   `rateCapacity` / `rateRefillPerSec` keys.
#[napi(js_name = "triggerServerSynthesis")]
pub fn js_trigger_server_synthesis(
    handle: BigInt,
    scope_id: String,
    tier: String,
) -> Result<String> {
    let h = handle_from_bigint(&handle)?;
    let typed: ffi::SynthesisTierKind = parse_arg(serde_json::Value::String(tier))?;
    crate::trigger_server_synthesis(h, scope_id, typed).map_err(to_js_error)
}

/// Look up the lifecycle state of a synthesis window. Mirrors
/// [`crate::synthesis_status`].
///
/// Returns the serialised [`ffi::SynthesisStatusRecord`] with
/// camelCase keys:
/// `{ synthesisId, scopeId, tier, status, windowStartUnix,
///    windowEndUnix, objectId }`.
///
/// # Errors
///
/// * `NotFound` if no window matches `synthesisId`.
/// * `InvalidArgument` if `synthesisId` is not a UUID.
#[napi(js_name = "synthesisStatus")]
pub fn js_synthesis_status(handle: BigInt, synthesis_id: String) -> Result<serde_json::Value> {
    let h = handle_from_bigint(&handle)?;
    let rec = crate::synthesis_status(h, synthesis_id).map_err(to_js_error)?;
    serde_json::to_value(rec).map_err(|e| {
        to_js_error(NapiError::Internal {
            message: format!("synthesis status serialization failed: {e}"),
        })
    })
}

/// Enumerate recent synthesis windows for `scopeId`. Mirrors
/// [`crate::list_recent_syntheses`]. Returns the rows sorted by
/// `windowEnd` descending and capped at
/// [`ffi::LIST_RECENT_SYNTHESES_CAP`].
///
/// Returns an empty array for a scope with no recorded synthesis
/// history — including the forgotten / unknown / never-touched
/// cases. This matches the underlying FFI's "soft" semantic and
/// avoids surfacing tombstone state to the host (mirroring how
/// `list_channel_facts` etc. handle forgotten scopes).
///
/// # Errors
///
/// * `InvalidArgument` if `scopeId` is not a UUID.
#[napi(js_name = "listRecentSyntheses")]
pub fn js_list_recent_syntheses(handle: BigInt, scope_id: String) -> Result<serde_json::Value> {
    let h = handle_from_bigint(&handle)?;
    let rows = crate::list_recent_syntheses(h, scope_id).map_err(to_js_error)?;
    serde_json::to_value(rows).map_err(|e| {
        to_js_error(NapiError::Internal {
            message: format!("synthesis list serialization failed: {e}"),
        })
    })
}

/// Re-run synthesis on an existing `Complete` window (Phase 10
/// Item 4). The window transitions back through `Complete →
/// Pending → InProgress → Complete` (or `→ Failed` on engine
/// error) on the same `(scope, window_id)` pair; the previous
/// synthesis object is archived to the history table at its
/// existing version stamp, and the new object lands at
/// `prior + 1`. Returns the post-replay synthesis status record
/// (versioned).
///
/// Bypasses the per-(scope, tier) cooldown but is still rate-
/// shaped through the FFI-wide token bucket — bursting replays
/// across many scopes will surface a `Throttled` error to the
/// host.
///
/// # Errors
///
/// * `InvalidId` if `scopeId` or `synthesisId` is not a valid
///   UUID.
/// * `NotFound` (`kind: "scope"`) if `scopeId` has been
///   forgotten.
/// * `NotFound` (`kind: "synthesis_window"`) if the substrate
///   does not know of a window with that id.
/// * `NotFound` (`kind: "synthesis_object"`) if the window has
///   no prior synthesis object to replay (e.g. it only ever
///   reached `Pending` or `Failed`).
/// * `Unavailable` if no engine is configured or `scopeId` is
///   not in the configured `scopeBindings` allow-list.
/// * `Throttled` if the FFI-wide rate limiter rejects the call.
/// * `Synthesis` if the window is not currently `Complete`
///   (replay refuses Pending / InProgress / Failed to avoid
///   racing in-flight dispatches), if the engine surfaced an
///   error, or if the response payload exceeds the configured
///   output cap.
/// * `Evidence` if persisting the new synthesis object /
///   archiving the prior version / updating the memory blob
///   fails.
#[napi(js_name = "replaySynthesis")]
pub fn js_replay_synthesis(
    handle: BigInt,
    scope_id: String,
    synthesis_id: String,
) -> Result<serde_json::Value> {
    let h = handle_from_bigint(&handle)?;
    let rec = crate::replay_synthesis(h, scope_id, synthesis_id).map_err(to_js_error)?;
    serde_json::to_value(rec).map_err(|e| {
        to_js_error(NapiError::Internal {
            message: format!("replay synthesis serialization failed: {e}"),
        })
    })
}

/// Enumerate the archived synthesis-object versions for
/// `synthesisId` (Phase 10 Item 4), newest first. The latest
/// version is included as the first entry with
/// `isLatest = true`. Hosts that need to paginate the history
/// without a separate `synthesisStatus` round trip should use
/// this surface.
///
/// Returns an empty array for a window with no prior synthesis
/// object (Pending / Failed-without-success window), matching
/// the "empty for unknown shape" convention used by
/// [`js_list_recent_syntheses`].
///
/// # Errors
///
/// * `InvalidArgument` if `synthesisId` is not a UUID.
#[napi(js_name = "listSynthesisVersions")]
pub fn js_list_synthesis_versions(
    handle: BigInt,
    synthesis_id: String,
) -> Result<serde_json::Value> {
    let h = handle_from_bigint(&handle)?;
    let rows = crate::list_synthesis_versions(h, synthesis_id).map_err(to_js_error)?;
    serde_json::to_value(rows).map_err(|e| {
        to_js_error(NapiError::Internal {
            message: format!("synthesis versions serialization failed: {e}"),
        })
    })
}

// ---------------------------------------------------------------------------
// Approved documents (Phase 8).
// ---------------------------------------------------------------------------

/// Admit an approved document onto the tenant memory for `scopeId`
/// and persist its AEAD-encrypted payload alongside. Mirrors
/// [`crate::admit_approved_document`].
///
/// `payload` is the raw plaintext document bytes; the substrate
/// seals it under the per-scope DEK before writing. The 16 MiB cap
/// is enforced inside the FFI layer — oversize payloads are
/// rejected with `Memory` and a message naming both the offending
/// size and the cap.
///
/// Returns the serialised [`ffi::ApprovedDocumentSummary`] with
/// camelCase keys:
/// `{ id, scopeId, label, approver, approvedAtMs, payloadBytes,
///    contentHashHex }`.
///
/// # Errors
///
/// * `NotFound` if the scope has no tenant memory object
///   registered (or has been forgotten).
/// * `Memory` if `label` / `approver` / `payload` are empty or
///   exceed their documented size caps.
/// * `InvalidArgument` if `scopeId` is not a UUID.
#[napi(js_name = "admitApprovedDocument")]
pub fn js_admit_approved_document(
    handle: BigInt,
    scope_id: String,
    label: String,
    approver: String,
    payload: napi::bindgen_prelude::Buffer,
) -> Result<serde_json::Value> {
    let h = handle_from_bigint(&handle)?;
    let bytes: Vec<u8> = payload.to_vec();
    let summary =
        crate::admit_approved_document(h, scope_id, label, approver, bytes).map_err(to_js_error)?;
    serde_json::to_value(summary).map_err(|e| {
        to_js_error(NapiError::Internal {
            message: format!("approved-document summary serialization failed: {e}"),
        })
    })
}

/// Replace the payload and metadata of an existing approved
/// document while keeping its `documentId` stable. Mirrors
/// [`crate::replace_approved_document`].
///
/// Use this when the host wants to publish a new revision of a
/// previously-admitted document without changing the id that
/// downstream synthesis / audit consumers reference. The
/// `documentId` and `scopeId` remain identical; `label`,
/// `approver`, `payload`, `contentHashHex`, `payloadBytes`, and
/// `approvedAtMs` are refreshed. A fresh `approvedAtMs` means the
/// replaced document is treated as "recently touched" by the
/// per-dispatch LRU cap.
///
/// Returns the serialised [`ffi::ApprovedDocumentSummary`] with
/// camelCase keys, identical in shape to
/// [`js_admit_approved_document`].
///
/// # Errors
///
/// * `NotFound { kind: "scope" }` if the scope has been forgotten
///   via `forgetScope`.
/// * `NotFound { kind: "tenant_memory" }` if no tenant memory
///   object exists for the scope (no document has ever been
///   admitted on it). Hosts must `admitApprovedDocument` first.
/// * `NotFound { kind: "approved_document" }` if `documentId` is
///   not currently admitted on this scope's tenant memory.
/// * `Memory` if `label` / `approver` / `payload` are empty or
///   exceed their documented size caps (see
///   [`crate::MAX_APPROVED_DOCUMENT_BYTES`] and
///   [`crate::MAX_APPROVED_DOCUMENT_METADATA_BYTES`]).
/// * `InvalidArgument` if `scopeId` or `documentId` is not a UUID.
///
/// The three-way `kind` distinction mirrors `revokeApprovedDocument`
/// so JS/TS hosts can pattern-match on `err.kind` uniformly across
/// both functions.
#[napi(js_name = "replaceApprovedDocument")]
pub fn js_replace_approved_document(
    handle: BigInt,
    scope_id: String,
    document_id: String,
    label: String,
    approver: String,
    payload: napi::bindgen_prelude::Buffer,
) -> Result<serde_json::Value> {
    let h = handle_from_bigint(&handle)?;
    let bytes: Vec<u8> = payload.to_vec();
    let summary =
        crate::replace_approved_document(h, scope_id, document_id, label, approver, bytes)
            .map_err(to_js_error)?;
    serde_json::to_value(summary).map_err(|e| {
        to_js_error(NapiError::Internal {
            message: format!("approved-document summary serialization failed: {e}"),
        })
    })
}

/// Revoke an approved document previously admitted via
/// [`js_admit_approved_document`]. Mirrors
/// [`crate::revoke_approved_document`]. Removes both the
/// tenant-memory ref and the persisted AEAD-encrypted payload row.
///
/// # Errors
///
/// * `NotFound` if the scope has been forgotten or no document
///   matches `documentId`.
/// * `InvalidArgument` if `scopeId` or `documentId` is not a UUID.
#[napi(js_name = "revokeApprovedDocument")]
pub fn js_revoke_approved_document(
    handle: BigInt,
    scope_id: String,
    document_id: String,
) -> Result<()> {
    let h = handle_from_bigint(&handle)?;
    crate::revoke_approved_document(h, scope_id, document_id).map_err(to_js_error)
}

/// List approved documents admitted onto the tenant memory for
/// `scopeId` along with persisted payload metadata. Mirrors
/// [`crate::list_approved_documents`].
///
/// Returns an empty array for a forgotten / never-touched scope
/// (matching the "soft" semantic shared with
/// [`js_list_recent_syntheses`]). Orphan refs (a tenant-memory ref
/// without a persisted payload row, e.g. from legacy admission
/// paths) surface with `payloadBytes = 0`.
///
/// # Errors
///
/// * `InvalidArgument` if `scopeId` is not a UUID.
#[napi(js_name = "listApprovedDocuments")]
pub fn js_list_approved_documents(handle: BigInt, scope_id: String) -> Result<serde_json::Value> {
    let h = handle_from_bigint(&handle)?;
    let rows = crate::list_approved_documents(h, scope_id).map_err(to_js_error)?;
    serde_json::to_value(rows).map_err(|e| {
        to_js_error(NapiError::Internal {
            message: format!("approved-document list serialization failed: {e}"),
        })
    })
}

/// Toggle the post-sync auto-synthesis hook for a connector
/// instance (Phase 7). Mirrors [`crate::configure_sync_auto_synthesize`].
///
/// When `enabled` is `true`, the scheduler dispatches a domain-tier
/// `triggerServerSynthesis` after every successful sync of this
/// instance, subject to the per-scope cooldown.
///
/// # Errors
///
/// * `Connector` if no scheduler is running on this handle.
/// * `InvalidArgument` if `instanceId` is not a UUID.
#[napi(js_name = "configureSyncAutoSynthesize")]
pub fn js_configure_sync_auto_synthesize(
    handle: BigInt,
    instance_id: String,
    enabled: bool,
) -> Result<()> {
    let h = handle_from_bigint(&handle)?;
    crate::configure_sync_auto_synthesize(h, instance_id, enabled).map_err(to_js_error)
}

// ---------------------------------------------------------------------------
// Cryptography
// ---------------------------------------------------------------------------

/// Generate a fresh signing keypair. Mirrors
/// [`crate::generate_keypair`]. The returned object has `algorithm`,
/// `publicKey`, `privateKey` fields (the key fields are arrays of
/// byte integers in `[0, 255]`).
#[napi(js_name = "generateKeypair")]
pub fn js_generate_keypair() -> Result<serde_json::Value> {
    let kp = crate::generate_keypair().map_err(to_js_error)?;
    serde_json::to_value(kp).map_err(|e| {
        // See `js_query` — a substrate-side encoding bug, not
        // a caller-side input bug.
        to_js_error(NapiError::Internal {
            message: format!("failed to serialise keypair: {e}"),
        })
    })
}

/// Encrypt a base64-encoded plaintext for a scope. Mirrors
/// [`crate::encrypt`].
#[napi(js_name = "encrypt")]
pub fn js_encrypt(handle: BigInt, scope_id: String, plaintext_b64: String) -> Result<String> {
    let h = handle_from_bigint(&handle)?;
    crate::encrypt(h, scope_id, plaintext_b64).map_err(to_js_error)
}

/// Decrypt a base64-encoded ciphertext envelope. Mirrors
/// [`crate::decrypt`].
#[napi(js_name = "decrypt")]
pub fn js_decrypt(handle: BigInt, scope_id: String, ciphertext_b64: String) -> Result<String> {
    let h = handle_from_bigint(&handle)?;
    crate::decrypt(h, scope_id, ciphertext_b64).map_err(to_js_error)
}

// ---------------------------------------------------------------------------
// Health / version surface (consumed by the desktop status panel and the
// `health-check` exit-code probe shipped alongside the addon).
// ---------------------------------------------------------------------------

/// Return the package version of the Rust core baked into this
/// `.node` artefact. Mirrors [`crate::core_version`]. Lets the JS-side
/// bootstrapper assert against a known-good version before opening
/// any stores so a stale addon from a previous install doesn't
/// silently corrupt data.
#[napi(js_name = "coreVersion")]
pub fn js_core_version() -> String {
    crate::core_version()
}

/// Full health envelope. Mirrors [`crate::health_check`].
///
/// `handle` is optional:
/// * pass `0n` (or omit it via the TypeScript optional parameter)
///   to get a bridge-only envelope — useful immediately after
///   loading the addon, before any [`js_open_store`] call.
/// * pass a [`BigInt`] returned by [`js_open_store`] to get a full
///   probe (`bridge` + `evidence_store` + `crypto` + `memory_manager`
///   + `inference_router` + `connector` subsystems, with real
///   per-subsystem I/O). The `connector` subsystem reports the
///   in-memory connector-instance count, authenticated-token count,
///   and per-`SyncStatus` distribution across all registered
///   connectors (downgrading to `Degraded` if any connector is in
///   `Failed`).
///
/// Returns the [`ffi::HealthStatus`] envelope serialised to JSON.
/// JS hosts get a typed object via napi's `serde_json::Value`
/// transparent passthrough; the shape is documented in
/// `crates/napi/index.d.ts`.
#[napi(js_name = "healthCheck")]
pub fn js_health_check(handle: Option<BigInt>) -> Result<serde_json::Value> {
    let h = match handle.as_ref() {
        Some(bi) => Some(handle_from_bigint(bi)?),
        None => None,
    };
    let envelope = crate::health_check(h).map_err(to_js_error)?;
    serde_json::to_value(envelope).map_err(|e| {
        to_js_error(crate::NapiError::Internal {
            message: format!("failed to serialize HealthStatus: {e}"),
        })
    })
}

// ---------------------------------------------------------------------------
// Connector management surface — mirrors the six connector FFI functions
// from `crates/ffi/src/connector.rs` through to the JS host. The JS-side
// argument shape is:
//
//   * `kind` arrives as a JS string matching the snake_case serde tags on
//     `ConnectorKindTag` (e.g. `"google_drive"`, `"notion"`, `"slack"`).
//   * `configJson` is a JS string (NOT a JS object) carrying the provider's
//     OAuth2 config (`client_id`, `redirect_uri`, `token_url`, etc.) so the
//     same exact bytes get parsed once on the Rust side, matching the
//     UniFFI signature on iOS / Android.
//
// Return shapes:
//
//   * `createConnector` returns the new instance UUID as a string.
//   * `syncConnector` returns the `SyncReport` envelope serialised to JSON
//     for ergonomic JS-side destructuring.
//   * `listConnectors` returns the same `ConnectorStatus[]` envelope.
// ---------------------------------------------------------------------------

/// Instantiate a connector. Mirrors [`crate::create_connector`].
#[napi(js_name = "createConnector")]
pub fn js_create_connector(
    handle: BigInt,
    kind: serde_json::Value,
    scope_id: String,
    config_json: String,
) -> Result<String> {
    let h = handle_from_bigint(&handle)?;
    let kind_tag: ConnectorKindTag = parse_arg(kind)?;
    crate::create_connector(h, kind_tag, scope_id, config_json).map_err(to_js_error)
}

/// Run the OAuth2 `authorization_code` exchange for an existing
/// connector instance. Mirrors [`crate::authenticate_connector`].
#[napi(js_name = "authenticateConnector")]
pub fn js_authenticate_connector(
    handle: BigInt,
    instance_id: String,
    auth_code: String,
) -> Result<()> {
    let h = handle_from_bigint(&handle)?;
    crate::authenticate_connector(h, instance_id, auth_code).map_err(to_js_error)
}

/// Run a connector sync and ingest emitted events into the evidence
/// store. Mirrors [`crate::sync_connector`]. Returns the
/// [`SyncReport`] envelope serialised as `serde_json::Value` for JS.
#[napi(js_name = "syncConnector")]
pub fn js_sync_connector(handle: BigInt, instance_id: String) -> Result<serde_json::Value> {
    let h = handle_from_bigint(&handle)?;
    let report: SyncReport = crate::sync_connector(h, instance_id).map_err(to_js_error)?;
    serde_json::to_value(report).map_err(|e| {
        to_js_error(NapiError::Internal {
            message: format!("failed to serialize SyncReport: {e}"),
        })
    })
}

/// List configured connector instances. Mirrors
/// [`crate::list_connectors`]. Returns a JSON array of
/// [`ConnectorStatus`] objects.
#[napi(js_name = "listConnectors")]
pub fn js_list_connectors(handle: BigInt) -> Result<serde_json::Value> {
    let h = handle_from_bigint(&handle)?;
    let rows: Vec<ConnectorStatus> = crate::list_connectors(h).map_err(to_js_error)?;
    serde_json::to_value(rows).map_err(|e| {
        to_js_error(NapiError::Internal {
            message: format!("failed to serialize ConnectorStatus list: {e}"),
        })
    })
}

/// Single-instance connector health probe (Phase 10 Item 3) —
/// symmetric with [`js_synthesis_status`]. Mirrors
/// [`crate::connector_status`] and returns a JSON object with the
/// shape:
///
/// `{ instanceId, kind, scopeId, syncMode, syncStatus,
///    lastSyncedAt, lastError, isScheduled, syncIntervalSecs,
///    maxBackoffSecs, autoSynthesize, consecutiveFailures,
///    nextAttemptUnix, inCooldown }`.
///
/// `isScheduled` is `true` iff `startSyncScheduler` is currently
/// running on this runtime; the scheduler-side fields
/// (`syncIntervalSecs`, `maxBackoffSecs`, `autoSynthesize`,
/// `consecutiveFailures`, `nextAttemptUnix`, `inCooldown`)
/// gracefully degrade to zero / `null` / `false` when the
/// scheduler is stopped. None of the per-instance policy state
/// survives a `stopSyncScheduler` / `startSyncScheduler` cycle —
/// the `SchedulePolicy` table lives inside the running scheduler
/// value and is dropped on stop. Hosts must re-apply
/// `configureSyncAutoSynthesize` / `configureSyncSchedule` after
/// each restart if they need their per-instance overrides back.
///
/// # Errors
///
/// * `NotFound` (`kind: "connector_instance"`) if `instanceId` is
///   a valid UUID but the runtime has no instance with that id
///   (host called [`js_remove_connector`] previously, or never
///   created one).
/// * `NotFound` (`kind: "scope"`) if the instance row exists but
///   its bound scope has been tombstoned by [`js_forget_scope`]
///   — same tombstoned-scope shield other connector surfaces
///   apply.
/// * `InvalidId` if `instanceId` is not a UUID.
/// * `Unavailable` if [`js_open_store`] has not been called.
#[napi(js_name = "connectorStatus")]
pub fn js_connector_status(handle: BigInt, instance_id: String) -> Result<serde_json::Value> {
    let h = handle_from_bigint(&handle)?;
    let rec: ConnectorHealthRecord =
        crate::connector_status(h, instance_id).map_err(to_js_error)?;
    serde_json::to_value(rec).map_err(|e| {
        to_js_error(NapiError::Internal {
            message: format!("failed to serialize ConnectorHealthRecord: {e}"),
        })
    })
}

/// Tear down a connector. Mirrors [`crate::remove_connector`].
#[napi(js_name = "removeConnector")]
pub fn js_remove_connector(handle: BigInt, instance_id: String) -> Result<()> {
    let h = handle_from_bigint(&handle)?;
    crate::remove_connector(h, instance_id).map_err(to_js_error)
}

/// Drive an OAuth2 `grant_type=refresh_token` round-trip against
/// the provider's token endpoint, persist the refreshed token to
/// SQLCipher, and update the per-runtime token vault. Mirrors
/// [`crate::refresh_connector_token`]. Returns the
/// [`RefreshReport`] envelope serialised as `serde_json::Value`
/// for JS so callers can destructure `{ instanceId, refreshed,
/// expiresAt, refreshedAt }` directly.
///
/// # Errors
///
/// * `Connector` carrying the framework's `TokenRefresh`
///   diagnostic when the provider rejects the refresh grant
///   (refresh token revoked / expired). The host should treat
///   this as "re-authorisation required" and prompt the user
///   through `authenticateConnector` rather than retrying the
///   refresh.
/// * `Connector` carrying `"no refresh_token stored …"` when the
///   cached token has no refresh token (Slack legacy, PKCE-only
///   public clients). Same recovery as above.
/// * `NotFound` (`kind = "connector" | "scope"`) when the
///   instance / scope was removed during the unlocked refresh
///   round-trip. (Pre-existing connector surfaces use the shorter
///   `"connector"` discriminant rather than `"connector_instance"`
///   used by newer surfaces such as `connectorStatus`.)
/// * `Unavailable` (`subsystem: "connector-http-client"`) when no
///   real HTTP transport is linked into the build.
#[napi(js_name = "refreshConnectorToken")]
pub fn js_refresh_connector_token(
    handle: BigInt,
    instance_id: String,
) -> Result<serde_json::Value> {
    let h = handle_from_bigint(&handle)?;
    let report: RefreshReport =
        crate::refresh_connector_token(h, instance_id).map_err(to_js_error)?;
    serde_json::to_value(report).map_err(|e| {
        to_js_error(NapiError::Internal {
            message: format!("failed to serialize RefreshReport: {e}"),
        })
    })
}

// ─────────────── OAuth2 client-secret resolver (Phase 4.1) ───────────────

/// Adapter that bridges a JS callback (passed across the N-API
/// boundary as a [`Function`]) into the substrate's
/// [`ffi::OAuthClientSecretResolver`] trait.
///
/// The Rust trait method [`resolve`] is invoked from connector worker
/// threads driving OAuth2 grants — never the JS main thread — so we
/// hand the JS callback off to a [`ThreadsafeFunction`] that
/// `napi-rs` will dispatch on the JS event loop. We then sync-wait
/// the worker thread on a `std::sync::mpsc` channel that the JS-side
/// callback fills in with the resolved secret.
///
/// Sync waits on JS from a worker thread are safe here because the
/// FFI substrate's three-phase locking pattern guarantees the
/// runtime mutex is NOT held while a grant is in flight — so the JS
/// event loop calling back into our other entry points cannot
/// deadlock on the worker thread's lock.
///
/// `tsfn` is `Send + Sync` (the napi-rs threadsafe-function API is
/// explicitly designed for cross-thread call), satisfying the
/// `OAuthClientSecretResolver: Send + Sync` requirement.
struct JsClientSecretResolver {
    tsfn: ThreadsafeFunction<
        (String, String, String),
        Option<String>,
        (String, String, String),
        napi::Status,
        false, // CalleeHandled — false: the JS-side resolver does NOT receive a leading (err, ...) Node-callback shape.
    >,
    /// Defense-in-depth ceiling on how long the worker thread will
    /// block on the JS event loop returning a resolver result.
    ///
    /// The framework treats the resolver as a SHOULD-be-cheap
    /// callback; the N-API adapter cannot enforce that contract on
    /// the JS side, so a host that ships a buggy resolver (infinite
    /// loop, deadlock on an unrelated JS lock, an event loop choked
    /// by other long-running work) would otherwise stall the
    /// connector worker thread permanently — and every subsequent
    /// grant would queue up behind it. After this timeout we
    /// abandon the wait and return `None`, which falls through to
    /// the framework's `auth_config_json` fallback layer and
    /// emits the WARN-once-per-`(kind, scope_id, client_id)`
    /// diagnostic on the framework side.
    ///
    /// The default value is set in [`Self::DEFAULT_RECV_TIMEOUT`]
    /// and is generous enough for any plausible cache /
    /// keychain-cached lookup (the framework's trait doc explicitly
    /// recommends an in-memory cache); production hosts can
    /// override via the optional `timeoutMs` argument to
    /// [`js_set_oauth_client_secret_resolver`].
    recv_timeout: std::time::Duration,
}

impl JsClientSecretResolver {
    /// Default ceiling for the JS resolver callback. Chosen to be
    /// short enough that a wedged JS event loop surfaces quickly
    /// (host operators get a WARN log within a few seconds rather
    /// than an indefinite stall) yet long enough that a cold
    /// keychain unlock + cache fill on a busy event loop still
    /// completes successfully on first call.
    const DEFAULT_RECV_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
}

/// Resolve the recv-timeout from an optional JS-side `timeoutMs`.
///
/// `None` → adapter default (5000 ms). `Some(0)` is rejected with
/// an `InvalidArgument`-kind [`NapiError`] because a zero timeout
/// would always time out before the JS event loop processed the
/// callback (a silent footgun masquerading as "no timeout"). The
/// rejection is input-validation, not a substrate-side encoding
/// failure — see [`NapiError::Internal`]'s docs at
/// `crates/napi/src/error.rs:34` for the contract that
/// distinguishes the two kinds. `Some(n)` converts `n` to
/// `Duration::from_millis(n)`.
///
/// Extracted into a `pub(crate)` helper so the validation logic is
/// unit-testable without standing up a live N-API environment to
/// construct a `Function` argument.
pub(crate) fn resolve_recv_timeout(
    timeout_ms: Option<u32>,
) -> std::result::Result<std::time::Duration, NapiError> {
    match timeout_ms {
        None => Ok(JsClientSecretResolver::DEFAULT_RECV_TIMEOUT),
        Some(0) => Err(NapiError::InvalidArgument {
            message: "setOauthClientSecretResolver: timeoutMs must be > 0 (a zero \
                      timeout would always time out before the JS event loop \
                      processed the callback); pass a positive value or omit \
                      to use the 5000 ms default"
                .into(),
        }),
        Some(n) => Ok(std::time::Duration::from_millis(u64::from(n))),
    }
}

impl ffi::OAuthClientSecretResolver for JsClientSecretResolver {
    fn resolve(&self, kind: String, scope_id: String, client_id: String) -> Option<String> {
        // `sync_channel(1)` gives a single-slot oneshot — JS-side
        // callback fills it with the resolved value (or `None` on
        // JS exception); the worker thread blocks on `recv()`.
        let (tx, rx) = std::sync::mpsc::sync_channel::<Option<String>>(1);
        let kind_for_warn = kind.clone();
        let scope_id_for_warn = scope_id.clone();
        let client_id_for_warn = client_id.clone();
        let status = self.tsfn.call_with_return_value(
            (kind, scope_id, client_id),
            ThreadsafeFunctionCallMode::Blocking,
            move |result: Result<Option<String>>, _env| {
                // A JS exception during the resolver call surfaces
                // as `Err(...)`; treat that the same as `None` so
                // the framework falls through to the
                // `auth_config_json["client_secret"]` fallback
                // layer. Per the trait contract, the resolver MUST
                // NOT crash the substrate on its own errors.
                let _ = tx.send(result.unwrap_or(None));
                Ok(())
            },
        );
        // `call_with_return_value` returns a Status enum; non-`Ok`
        // means the call was queued unsuccessfully (e.g. the JS
        // thread is closing). Surface that as `None` so the
        // framework falls through to the fallback layer.
        if status != napi::Status::Ok {
            return None;
        }
        // `recv_timeout` bounds the wait — see
        // [`Self::recv_timeout`]'s doc comment for the rationale.
        // The two error variants are handled differently:
        //
        // * `Timeout`: JS event loop did not deliver the result
        //   within the budget. Emit a WARN so the operator can
        //   correlate this with the framework's per-instance
        //   `client_secret unavailable` WARN, then return `None`
        //   so the framework's fallback ladder continues.
        // * `Disconnected`: the sending half was dropped without
        //   sending — this is the pre-existing path when the
        //   tsfn is aborted (e.g. process shutdown). Stay silent
        //   in that case, because the resolver-was-aborted signal
        //   is uninteresting under teardown.
        match rx.recv_timeout(self.recv_timeout) {
            Ok(secret) => secret,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // `Duration::as_millis` returns u128 to handle the
                // theoretical max-Duration range; the configured
                // `recv_timeout` is sourced from a `u32` ms value
                // (see `resolve_recv_timeout`) so the value
                // ALWAYS fits in u64 — but use `try_into` +
                // saturating fallback rather than `as` to stay
                // friendly with clippy's truncation lint.
                let timeout_ms_for_log: u64 =
                    self.recv_timeout.as_millis().try_into().unwrap_or(u64::MAX);
                tracing::warn!(
                    kind = %kind_for_warn,
                    scope_id = %scope_id_for_warn,
                    client_id = %client_id_for_warn,
                    timeout_ms = timeout_ms_for_log,
                    "JS OAuth2 client_secret resolver did not return within timeout; \
                     falling through to auth_config_json / static fallback. Hosts \
                     should either make the resolver cheaper (in-memory cache) or \
                     raise `timeoutMs` when calling setOauthClientSecretResolver.",
                );
                None
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => None,
        }
    }
}

/// Register a host-supplied OAuth2 client-secret resolver against
/// `handle`'s per-runtime [`ffi::OAuth2Client`]. Mirrors
/// [`crate::set_oauth_client_secret_resolver`].
///
/// `resolver` is a JS function with the signature
/// `(kind: string, scopeId: string, clientId: string) =>
/// string | null | undefined`. Returning `null` / `undefined`
/// (both map to `None` on the Rust side) defers to the next
/// layer of the framework's fallback ladder. Returning `""`
/// (an empty string) is treated as an **explicit "no-secret"
/// choice** and short-circuits both the `auth_config_json`
/// fallback and any static `with_client_secret` value — the
/// substrate then omits the `client_secret` form field entirely
/// (public-client semantics). Returning a non-empty string uses
/// that secret. See the trait-level rustdoc on
/// [`ffi::OAuthClientSecretResolver`] for the full semantics and
/// [`connector_framework::OAuth2Client::client_secret_for`] for
/// the resolution ladder.
///
/// The JS callback runs on the JS event loop (the substrate's
/// connector worker threads `await` the result via a
/// [`ThreadsafeFunction`]); implementations are free to look the
/// secret up from the OS keychain, an in-memory cache, or any
/// async source as long as the call returns synchronously to the
/// substrate's perspective.
///
/// `timeoutMs` is an optional defense-in-depth ceiling on how long
/// the connector worker thread will block waiting for the JS
/// resolver callback to deliver its result. When exceeded the
/// adapter logs a WARN with `(kind, scopeId, clientId,
/// timeout_ms)` and returns `None` to the framework — which then
/// falls through to the `auth_config_json["client_secret"]`
/// fallback layer just as if the resolver had returned `null`.
/// This prevents a wedged JS event loop (infinite loop in user
/// code, deadlock on another JS lock, etc.) from stalling the
/// connector worker thread permanently.
///
/// Omit `timeoutMs` (or pass `null`/`undefined`) to use the
/// adapter's default ceiling (5000 ms) — generous enough for any
/// in-memory cache / keychain-cached lookup. Pass a larger value
/// only if a slow cold-path lookup is expected at startup (e.g.
/// a keychain unlock that prompts the OS for the user's password).
/// A value of `0` is rejected as ambiguous (would always time out
/// before the JS event loop drained); pass an explicitly-large
/// value (e.g. 60_000) for "effectively unbounded" semantics.
///
/// Calling this multiple times REPLACES the previously-registered
/// resolver. Hosts typically call this exactly once per
/// `open_store` lifecycle.
#[napi(js_name = "setOauthClientSecretResolver")]
pub fn js_set_oauth_client_secret_resolver(
    handle: BigInt,
    resolver: Function<(String, String, String), Option<String>>,
    timeout_ms: Option<u32>,
) -> Result<()> {
    let h = handle_from_bigint(&handle)?;
    let recv_timeout = resolve_recv_timeout(timeout_ms).map_err(to_js_error)?;
    // Build a threadsafe function from the JS callback. The
    // builder defaults to `CalleeHandled = false` which matches
    // our adapter's type annotation.
    let tsfn = resolver
        .build_threadsafe_function::<(String, String, String)>()
        .callee_handled::<false>()
        .build()
        .map_err(|e| {
            to_js_error(NapiError::Internal {
                message: format!("failed to build threadsafe resolver: {e}"),
            })
        })?;
    let arc: std::sync::Arc<dyn ffi::OAuthClientSecretResolver> =
        std::sync::Arc::new(JsClientSecretResolver { tsfn, recv_timeout });
    crate::set_oauth_client_secret_resolver(h, arc).map_err(to_js_error)
}

/// Unregister the previously-registered OAuth2 client-secret
/// resolver on `handle`. Mirrors
/// [`crate::clear_oauth_client_secret_resolver`].
///
/// Calling this when no resolver is registered is a no-op.
#[napi(js_name = "clearOauthClientSecretResolver")]
pub fn js_clear_oauth_client_secret_resolver(handle: BigInt) -> Result<()> {
    let h = handle_from_bigint(&handle)?;
    crate::clear_oauth_client_secret_resolver(h).map_err(to_js_error)
}

// ─────────────── Master-key storage resolver (Track B-2) ────────────────

/// Adapter that bridges a JS resolver object (with three callable
/// methods: `loadKey`, `storeKey`, `deleteKey`) into the substrate's
/// [`ffi::KeyStorageResolver`] trait.
///
/// The Rust trait methods are invoked from the FFI cold-boot path
/// ([`crate::open_store_with_resolver`]) and the mid-life rotation
/// path — never the JS main thread — so each JS callback is held as
/// a [`ThreadsafeFunction`] which `napi-rs` will dispatch on the JS
/// event loop. We then sync-wait the substrate caller on a
/// `std::sync::mpsc` channel that the JS-side callback fills in.
///
/// Sync waits on JS from a worker / FFI caller thread are safe here
/// because the substrate's three-phase locking pattern guarantees
/// the runtime mutex is NOT held while a resolver call is in flight
/// — the resolver dispatch happens before `with_runtime` is entered
/// (on `open_store_with_resolver`) or outside its scope (on
/// `set_key_storage_resolver`). So the JS event loop calling back
/// into other entry points cannot deadlock on the substrate's lock.
///
/// `ThreadsafeFunction` is `Send + Sync` (the napi-rs threadsafe-
/// function API is explicitly designed for cross-thread call),
/// satisfying the `KeyStorageResolver: Send + Sync` requirement.
struct JsKeyStorageResolver {
    /// `loadKey(keyId: string) -> string | null | undefined`.
    ///
    /// Returns the hex-encoded 32-byte master key on success.
    /// Returns `null` / `undefined` to signal `NotFound`. A JS
    /// exception or non-string return collapses to
    /// `FfiError::Unavailable` so the substrate can distinguish a
    /// host-side misconfiguration (e.g. the JS callback threw on
    /// Keychain unlock denial) from a clean "no such key" miss.
    load_key_tsfn: ThreadsafeFunction<(String,), Option<String>, (String,), napi::Status, false>,
    /// `storeKey(keyId: string, keyHex: string) -> void`.
    ///
    /// Resolves to `Ok(())` if the call returned normally (any
    /// JS return value is treated as success — the substrate
    /// ignores it). A JS exception maps to
    /// `FfiError::Unavailable`.
    store_key_tsfn:
        ThreadsafeFunction<(String, String), Option<String>, (String, String), napi::Status, false>,
    /// `deleteKey(keyId: string) -> void`.
    ///
    /// Resolves to `Ok(())` if the call returned normally. A JS
    /// exception maps to `FfiError::Unavailable`. Per the trait
    /// contract, deleting an unknown id is a success, not an
    /// error — so the substrate does NOT re-tag this as
    /// `NotFound` even if the host's resolver does (idempotent
    /// delete is the trait's documented behaviour).
    delete_key_tsfn: ThreadsafeFunction<(String,), Option<String>, (String,), napi::Status, false>,
    /// Defense-in-depth ceiling on how long the substrate will
    /// block on the JS event loop returning a resolver result.
    ///
    /// Matches [`JsClientSecretResolver`]'s rationale: a host that
    /// ships a buggy resolver (infinite loop, deadlock on an
    /// unrelated JS lock, an event loop choked by other long-
    /// running work) would otherwise stall the FFI cold-boot
    /// indefinitely. After this timeout the adapter abandons the
    /// wait and surfaces `FfiError::Unavailable { subsystem:
    /// "host-key-store: <method> timed out" }` to the substrate.
    ///
    /// The default value is [`Self::DEFAULT_RECV_TIMEOUT`] and is
    /// generous enough for a cold keychain unlock that prompts the
    /// OS for the user's password. Production hosts can override
    /// via the optional `timeoutMs` argument to
    /// [`js_set_key_storage_resolver`] /
    /// [`js_open_store_with_resolver`].
    recv_timeout: std::time::Duration,
}

impl JsKeyStorageResolver {
    /// Default ceiling for each JS resolver callback. Chosen to
    /// be long enough for a cold keychain unlock that prompts the
    /// OS for biometric / password input (these prompts can take
    /// a user 10–20 seconds to satisfy in the worst case) yet
    /// short enough that a genuinely wedged JS event loop
    /// surfaces visibly rather than hanging the FFI forever.
    /// 30000 ms is the same ceiling used by the OAuth secret
    /// resolver in spirit, but bumped from 5 s → 30 s because
    /// master-key unlocks involve user interaction whereas
    /// client-secret resolution is meant to be a hot-cache hit.
    const DEFAULT_RECV_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
}

/// Resolve the recv-timeout for the key-storage resolver from an
/// optional JS-side `timeoutMs`.
///
/// `None` → adapter default (30000 ms). `Some(0)` is rejected
/// with an `InvalidArgument`-kind [`NapiError`] for the same
/// reason as [`resolve_recv_timeout`] (a zero timeout would
/// always time out before the JS event loop processed the
/// callback). `Some(n)` converts `n` to `Duration::from_millis(n)`.
///
/// Extracted into a `pub(crate)` helper so the validation logic
/// is unit-testable without standing up a live N-API environment.
pub(crate) fn resolve_key_storage_recv_timeout(
    timeout_ms: Option<u32>,
) -> std::result::Result<std::time::Duration, NapiError> {
    match timeout_ms {
        None => Ok(JsKeyStorageResolver::DEFAULT_RECV_TIMEOUT),
        Some(0) => Err(NapiError::InvalidArgument {
            message: "key storage resolver: timeoutMs must be > 0 (a zero \
                      timeout would always time out before the JS event loop \
                      processed the callback); pass a positive value or omit \
                      to use the 30000 ms default"
                .into(),
        }),
        Some(n) => Ok(std::time::Duration::from_millis(u64::from(n))),
    }
}

/// Extract a named `Function` property from a JS object, returning
/// a typed `NapiError::InvalidArgument` when the property is
/// missing. The four call sites
/// (`js_set_key_storage_resolver` and `js_open_store_with_resolver`,
/// each extracting three methods) all need identical
/// "missing-method" diagnostics, so the helper centralises that
/// validation.
///
/// `entry_point` is the calling JS function name (e.g.
/// `"setKeyStorageResolver"`) and is embedded into the error
/// message so the host knows which call site rejected.
fn extract_resolver_method<'env, Args, Return>(
    obj: &Object<'env>,
    method_name: &str,
    entry_point: &str,
) -> std::result::Result<Function<'env, Args, Return>, NapiError>
where
    Args: napi::bindgen_prelude::JsValuesTupleIntoVec,
    Return: napi::bindgen_prelude::FromNapiValue,
    Function<'env, Args, Return>: napi::bindgen_prelude::FromNapiValue,
{
    match obj.get::<Function<'env, Args, Return>>(method_name) {
        Ok(Some(f)) => Ok(f),
        Ok(None) => Err(NapiError::InvalidArgument {
            message: format!(
                "{entry_point}: resolver object is missing required \
                 `{method_name}` method (expected a function, found nothing)"
            ),
        }),
        Err(e) => Err(NapiError::InvalidArgument {
            message: format!(
                "{entry_point}: resolver object's `{method_name}` property \
                 is not a function: {e}"
            ),
        }),
    }
}

/// Build a [`JsKeyStorageResolver`] from a JS object with `loadKey`,
/// `storeKey`, `deleteKey` methods + an optional `timeoutMs`.
/// Shared by `setKeyStorageResolver` and `openStoreWithResolver`
/// so the JS shape is consistent across both entry points and the
/// extraction errors look identical.
fn build_js_key_storage_resolver(
    resolver: &Object<'_>,
    timeout_ms: Option<u32>,
    entry_point: &str,
) -> Result<JsKeyStorageResolver> {
    let recv_timeout = resolve_key_storage_recv_timeout(timeout_ms).map_err(to_js_error)?;

    let load_key_fn =
        extract_resolver_method::<(String,), Option<String>>(resolver, "loadKey", entry_point)
            .map_err(to_js_error)?;
    let store_key_fn = extract_resolver_method::<(String, String), Option<String>>(
        resolver,
        "storeKey",
        entry_point,
    )
    .map_err(to_js_error)?;
    let delete_key_fn =
        extract_resolver_method::<(String,), Option<String>>(resolver, "deleteKey", entry_point)
            .map_err(to_js_error)?;

    let load_key_tsfn = load_key_fn
        .build_threadsafe_function::<(String,)>()
        .callee_handled::<false>()
        .build()
        .map_err(|e| {
            to_js_error(NapiError::Internal {
                message: format!("{entry_point}: failed to build loadKey tsfn: {e}"),
            })
        })?;
    let store_key_tsfn = store_key_fn
        .build_threadsafe_function::<(String, String)>()
        .callee_handled::<false>()
        .build()
        .map_err(|e| {
            to_js_error(NapiError::Internal {
                message: format!("{entry_point}: failed to build storeKey tsfn: {e}"),
            })
        })?;
    let delete_key_tsfn = delete_key_fn
        .build_threadsafe_function::<(String,)>()
        .callee_handled::<false>()
        .build()
        .map_err(|e| {
            to_js_error(NapiError::Internal {
                message: format!("{entry_point}: failed to build deleteKey tsfn: {e}"),
            })
        })?;

    Ok(JsKeyStorageResolver {
        load_key_tsfn,
        store_key_tsfn,
        delete_key_tsfn,
        recv_timeout,
    })
}

impl ffi::KeyStorageResolver for JsKeyStorageResolver {
    fn load_key(&self, key_id: String) -> ffi::FfiResult<String> {
        // `sync_channel(1)` gives a single-slot oneshot — the JS
        // callback fills it with `Ok(hex_string)`, `Ok(None)` for
        // NotFound, or the napi error for a JS exception.
        let (tx, rx) = std::sync::mpsc::sync_channel::<Result<Option<String>>>(1);
        let key_id_for_call = key_id.clone();
        let status = self.load_key_tsfn.call_with_return_value(
            (key_id_for_call,),
            ThreadsafeFunctionCallMode::Blocking,
            move |result: Result<Option<String>>, _env| {
                let _ = tx.send(result);
                Ok(())
            },
        );
        if status != napi::Status::Ok {
            return Err(FfiError::Unavailable {
                subsystem: format!(
                    "host-key-store: loadKey dispatch failed with napi status {status:?}"
                ),
            });
        }
        match rx.recv_timeout(self.recv_timeout) {
            // Hex string returned — substrate validates the hex
            // shape further down the cold-boot path via
            // `parse_master_key_hex`. We deliberately do NOT
            // pre-validate here so the surface error mapping
            // (InvalidId vs Unavailable) is owned by exactly one
            // place in the codebase.
            Ok(Ok(Some(hex))) => Ok(hex),
            // `null` / `undefined` — clean miss, host doesn't
            // know about this id. The substrate's
            // `open_store_with_resolver` then re-tags this
            // `NotFound { kind: "key" }` as
            // `NotFound { kind: "master_key" }` for the cold-
            // boot path.
            Ok(Ok(None)) => Err(FfiError::NotFound {
                kind: "key".into(),
                id: key_id,
            }),
            // JS exception inside the callback. Surface as
            // `Unavailable` so the host can pattern-match on
            // `Unavailable { subsystem }` vs `NotFound`.
            Ok(Err(e)) => {
                tracing::warn!(
                    key_id = %key_id,
                    error = %e,
                    "JS key-storage resolver loadKey threw; surfacing as Unavailable",
                );
                Err(FfiError::Unavailable {
                    subsystem: format!("host-key-store: loadKey threw: {e}"),
                })
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                let timeout_ms: u64 = self.recv_timeout.as_millis().try_into().unwrap_or(u64::MAX);
                tracing::warn!(
                    key_id = %key_id,
                    timeout_ms,
                    "JS key-storage resolver loadKey did not return within timeout",
                );
                Err(FfiError::Unavailable {
                    subsystem: format!("host-key-store: loadKey timed out after {timeout_ms}ms"),
                })
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                // The sending half was dropped without sending
                // — the tsfn was aborted (e.g. process teardown).
                Err(FfiError::Unavailable {
                    subsystem: "host-key-store: loadKey dispatch aborted (tsfn closed)".into(),
                })
            }
        }
    }

    fn store_key(&self, key_id: String, key_hex: String) -> ffi::FfiResult<()> {
        let (tx, rx) = std::sync::mpsc::sync_channel::<Result<Option<String>>>(1);
        let key_id_for_warn = key_id.clone();
        let status = self.store_key_tsfn.call_with_return_value(
            (key_id, key_hex),
            ThreadsafeFunctionCallMode::Blocking,
            move |result: Result<Option<String>>, _env| {
                let _ = tx.send(result);
                Ok(())
            },
        );
        if status != napi::Status::Ok {
            return Err(FfiError::Unavailable {
                subsystem: format!(
                    "host-key-store: storeKey dispatch failed with napi status {status:?}"
                ),
            });
        }
        match rx.recv_timeout(self.recv_timeout) {
            // Any return value (string, null, undefined) is treated
            // as success — `void` JS functions return `undefined`
            // which deserialises as `None`. The substrate doesn't
            // consume the return value.
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) => {
                tracing::warn!(
                    key_id = %key_id_for_warn,
                    error = %e,
                    "JS key-storage resolver storeKey threw; surfacing as Unavailable",
                );
                Err(FfiError::Unavailable {
                    subsystem: format!("host-key-store: storeKey threw: {e}"),
                })
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                let timeout_ms: u64 = self.recv_timeout.as_millis().try_into().unwrap_or(u64::MAX);
                tracing::warn!(
                    key_id = %key_id_for_warn,
                    timeout_ms,
                    "JS key-storage resolver storeKey did not return within timeout",
                );
                Err(FfiError::Unavailable {
                    subsystem: format!("host-key-store: storeKey timed out after {timeout_ms}ms"),
                })
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(FfiError::Unavailable {
                subsystem: "host-key-store: storeKey dispatch aborted (tsfn closed)".into(),
            }),
        }
    }

    fn delete_key(&self, key_id: String) -> ffi::FfiResult<()> {
        let (tx, rx) = std::sync::mpsc::sync_channel::<Result<Option<String>>>(1);
        let key_id_for_call = key_id.clone();
        let key_id_for_warn = key_id;
        let status = self.delete_key_tsfn.call_with_return_value(
            (key_id_for_call,),
            ThreadsafeFunctionCallMode::Blocking,
            move |result: Result<Option<String>>, _env| {
                let _ = tx.send(result);
                Ok(())
            },
        );
        if status != napi::Status::Ok {
            return Err(FfiError::Unavailable {
                subsystem: format!(
                    "host-key-store: deleteKey dispatch failed with napi status {status:?}"
                ),
            });
        }
        match rx.recv_timeout(self.recv_timeout) {
            // Delete is idempotent per the trait contract: any
            // normal return (including "no such id") is success.
            // The host's JS resolver MUST follow the same
            // contract — i.e. don't throw on a missing-id delete,
            // just return.
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) => {
                tracing::warn!(
                    key_id = %key_id_for_warn,
                    error = %e,
                    "JS key-storage resolver deleteKey threw; surfacing as Unavailable",
                );
                Err(FfiError::Unavailable {
                    subsystem: format!("host-key-store: deleteKey threw: {e}"),
                })
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                let timeout_ms: u64 = self.recv_timeout.as_millis().try_into().unwrap_or(u64::MAX);
                tracing::warn!(
                    key_id = %key_id_for_warn,
                    timeout_ms,
                    "JS key-storage resolver deleteKey did not return within timeout",
                );
                Err(FfiError::Unavailable {
                    subsystem: format!("host-key-store: deleteKey timed out after {timeout_ms}ms"),
                })
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(FfiError::Unavailable {
                subsystem: "host-key-store: deleteKey dispatch aborted (tsfn closed)".into(),
            }),
        }
    }
}

/// Register a host-supplied master-key storage resolver against
/// `handle`'s per-runtime slot. Mirrors
/// [`crate::set_key_storage_resolver`].
///
/// `resolver` is a JS object with three callable methods:
///
/// * `loadKey(keyId: string) -> string | null | undefined` — return
///   the hex-encoded 32-byte master key, or `null` / `undefined` to
///   signal `NotFound`. The substrate's `open_store_with_resolver`
///   re-tags the `NotFound { kind: "key" }` it sees here as
///   `NotFound { kind: "master_key" }` for the cold-boot path so the
///   host can distinguish a master-key provisioning miss from a
///   generic key-id miss surfaced by future resolver call sites.
/// * `storeKey(keyId: string, keyHex: string) -> void` — persist
///   `keyHex` under `keyId`. Throw on persistence failure. The
///   substrate ignores the return value.
/// * `deleteKey(keyId: string) -> void` — drop the key registered
///   under `keyId`. MUST be idempotent — deleting a missing id is a
///   success, not an exception. The substrate ignores the return
///   value.
///
/// Any JS exception in a callback surfaces to the substrate as
/// [`ffi::FfiError::Unavailable`] with `subsystem` containing the
/// method name and the exception message. The substrate does NOT
/// re-tag JS exceptions as `NotFound` — that variant is reserved
/// for the explicit `null` / `undefined` return from `loadKey`.
///
/// `timeoutMs` is an optional defense-in-depth ceiling on how long
/// the substrate will block on a JS callback returning. Default is
/// 30000 ms (long enough for a cold keychain unlock that prompts the
/// OS for biometric / password input). Pass `0` is rejected as
/// ambiguous (would always time out); pass a larger value
/// (e.g. 60_000 or 120_000) for slow cold-path lookups.
///
/// Calling this multiple times REPLACES the previously-registered
/// resolver. Hosts typically call this exactly once per
/// `open_store` lifecycle.
///
/// This is the **mid-life registration path**. For the cold-boot
/// integration point that consumes `loadKey` to derive the master
/// key during `open_store`, see [`js_open_store_with_resolver`].
#[napi(js_name = "setKeyStorageResolver")]
pub fn js_set_key_storage_resolver(
    handle: BigInt,
    resolver: Object<'_>,
    timeout_ms: Option<u32>,
) -> Result<()> {
    let h = handle_from_bigint(&handle)?;
    let adapter = build_js_key_storage_resolver(&resolver, timeout_ms, "setKeyStorageResolver")?;
    let arc: std::sync::Arc<dyn ffi::KeyStorageResolver> = std::sync::Arc::new(adapter);
    crate::set_key_storage_resolver(h, arc).map_err(to_js_error)
}

/// Unregister the previously-registered master-key storage
/// resolver on `handle`. Mirrors
/// [`crate::clear_key_storage_resolver`].
///
/// Calling this when no resolver is registered is a no-op (the
/// trait's documented "last-write-wins" semantics).
#[napi(js_name = "clearKeyStorageResolver")]
pub fn js_clear_key_storage_resolver(handle: BigInt) -> Result<()> {
    let h = handle_from_bigint(&handle)?;
    crate::clear_key_storage_resolver(h).map_err(to_js_error)
}

/// Open the SQLCipher-backed evidence store at `path` using a
/// master key fetched from a host-supplied resolver object
/// (instead of passing the hex string directly to
/// [`js_open_store`]). Mirrors [`crate::open_store_with_resolver`].
///
/// This is the cold-boot integration point hardware-backed hosts
/// SHOULD use so the master key never enters the host's address
/// space as a long-lived plaintext hex string — the resolver pulls
/// it from Keychain / Keystore / DPAPI / TEE on demand, the
/// substrate consumes it and stashes the resolver on the runtime
/// so subsequent operations reach the same backing store, and the
/// resolver is dropped when [`js_close_store`] tears the runtime
/// down.
///
/// `resolver` follows the same JS shape as
/// [`js_set_key_storage_resolver`] (three methods: `loadKey`,
/// `storeKey`, `deleteKey`). On this call the substrate invokes
/// `loadKey(keyId)` exactly once.
///
/// Error mapping (see [`ffi::open_store_with_resolver`] for the
/// authoritative contract):
///
/// * `loadKey` returns `null` / `undefined` → `NotFound { kind:
///   "master_key", id: <keyId> }` (re-tagged from the
///   resolver's own `NotFound { kind: "key" }`).
/// * `loadKey` returns a string that is not 64 lowercase hex
///   chars → `InvalidId`.
/// * `loadKey` throws a JS exception → `Unavailable { subsystem:
///   "host-key-store: loadKey threw: ..." }`.
/// * `loadKey` does not return within `timeoutMs` →
///   `Unavailable { subsystem: "host-key-store: loadKey timed
///   out after Xms" }`.
/// * SQLCipher fails to open the underlying database with the
///   resolved key → `Evidence` (same as
///   [`js_open_store`]).
///
/// Returns the same opaque `BigInt` handle shape as
/// [`js_open_store`].
#[napi(js_name = "openStoreWithResolver")]
pub fn js_open_store_with_resolver(
    path: String,
    key_id: String,
    resolver: Object<'_>,
    timeout_ms: Option<u32>,
) -> Result<BigInt> {
    let adapter = build_js_key_storage_resolver(&resolver, timeout_ms, "openStoreWithResolver")?;
    let arc: std::sync::Arc<dyn ffi::KeyStorageResolver> = std::sync::Arc::new(adapter);
    let handle = crate::open_store_with_resolver(path, key_id, arc).map_err(to_js_error)?;
    Ok(BigInt::from(handle))
}

// ───────────────────────── Webhook receiver (Phase 5) ─────────────

/// Start a webhook receiver server bound to `bindAddr` (parsed as
/// a `SocketAddr` — `"127.0.0.1:9001"`, `"0.0.0.0:0"` for an
/// ephemeral port, `"[::1]:9001"` for IPv6). Mirrors
/// [`crate::start_webhook_server`]. Returns the opaque server
/// handle as a `BigInt`.
///
/// # Errors
///
/// * `Unavailable` if `open_store(handle)` has not yet been called.
/// * `InvalidArgument` if `bindAddr` is not a valid `host:port`
///   string.
/// * `Connector` if the OS rejects the bind (port in use,
///   permission denied) or the tokio runtime fails to spin up.
#[napi(js_name = "startWebhookServer")]
pub fn js_start_webhook_server(handle: BigInt, bind_addr: String) -> Result<BigInt> {
    let h = handle_from_bigint(&handle)?;
    let server_handle = crate::start_webhook_server(h, bind_addr).map_err(to_js_error)?;
    Ok(BigInt {
        sign_bit: false,
        words: vec![server_handle.0],
    })
}

/// Stop a previously-started webhook server and synchronously join
/// its runtime thread. Mirrors [`crate::stop_webhook_server`].
///
/// Idempotent — stopping an unknown / already-stopped server
/// returns success with no work.
///
/// # Errors
///
/// * `Unavailable` if `open_store(handle)` has not yet been called.
/// * `InvalidArgument` if `serverHandle` cannot be represented as
///   a u64.
#[napi(js_name = "stopWebhookServer")]
pub fn js_stop_webhook_server(handle: BigInt, server_handle: BigInt) -> Result<()> {
    let h = handle_from_bigint(&handle)?;
    let sh = ffi::WebhookServerHandle(handle_from_bigint(&server_handle)?);
    crate::stop_webhook_server(h, sh).map_err(to_js_error)
}

/// Bind `providerId` (one of the framework's recognised connector
/// slugs — `"slack"`, `"notion"`, …) to `instanceId` on the
/// running `serverHandle`. Mirrors
/// [`crate::register_webhook_dispatch`].
///
/// Re-registering an already-bound `providerId` REPLACES the prior
/// binding (idempotent).
///
/// # Errors
///
/// * `Unavailable` if `open_store(handle)` has not yet been called.
/// * `InvalidArgument` if `instanceId` is not a valid UUID.
/// * `NotFound` if `serverHandle` or `instanceId` does not name a
///   live entity.
/// * `Connector` if `providerId` is not one of the framework's
///   recognised connector slugs.
#[napi(js_name = "registerWebhookDispatch")]
pub fn js_register_webhook_dispatch(
    handle: BigInt,
    server_handle: BigInt,
    provider_id: String,
    instance_id: String,
) -> Result<()> {
    let h = handle_from_bigint(&handle)?;
    let sh = ffi::WebhookServerHandle(handle_from_bigint(&server_handle)?);
    crate::register_webhook_dispatch(h, sh, provider_id, instance_id).map_err(to_js_error)
}

/// Drop the binding for `(serverHandle, providerId)`. Mirrors
/// [`crate::unregister_webhook_dispatch`]. Idempotent.
///
/// # Errors
///
/// * `Unavailable` if `open_store(handle)` has not yet been called.
/// * `NotFound` if `serverHandle` does not name a running server.
#[napi(js_name = "unregisterWebhookDispatch")]
pub fn js_unregister_webhook_dispatch(
    handle: BigInt,
    server_handle: BigInt,
    provider_id: String,
) -> Result<()> {
    let h = handle_from_bigint(&handle)?;
    let sh = ffi::WebhookServerHandle(handle_from_bigint(&server_handle)?);
    crate::unregister_webhook_dispatch(h, sh, provider_id).map_err(to_js_error)
}

/// Enumerate every running webhook server on `handle` with its
/// per-server counters. Mirrors [`crate::list_webhook_servers`].
/// Returns a `serde_json::Value` (`Vec<WebhookServerSummary>`) so
/// callers can destructure
/// `{ serverHandle, bindAddr, startedAt, registrationCount,
///   dispatchOkTotal, dispatchBadRequestTotal, dispatchBadGatewayTotal }`
/// shapes without an additional `JSON.parse`.
///
/// # JS handle representation
///
/// `serverHandle` is serialised as a JSON `number`, not a
/// `BigInt` — consistent with the rest of the N-API `list*` family
/// (`listConnectors`, `listConnectorAuthState`, …) which all route
/// through `serde_json::to_value`. By contrast,
/// [`js_start_webhook_server`] returns the freshly-minted handle as
/// a `BigInt` (the canonical FFI representation). Hosts comparing
/// a handle taken from this list against one returned by
/// `startWebhookServer` must coerce to a common type: either widen
/// the list value via `BigInt(row.serverHandle)` or narrow the
/// start return via `Number(handle)`. Loose `==` works across the
/// `BigInt`/`Number` pair but strict `===` does NOT. Handles start
/// at `1` and increment monotonically (`u64` allocator at
/// `crates/ffi/src/webhook.rs::next_server_handle`); JavaScript
/// `Number` preserves integer precision up to `2^53 − 1`, so a
/// `number` representation is exact for any realistic substrate
/// uptime, but the `BigInt`-vs-`number` asymmetry is still a real
/// pitfall worth pinning at the call site.
///
/// # Errors
///
/// * `Unavailable` if `open_store(handle)` has not yet been called.
#[napi(js_name = "listWebhookServers")]
pub fn js_list_webhook_servers(handle: BigInt) -> Result<serde_json::Value> {
    let h = handle_from_bigint(&handle)?;
    let summaries: Vec<ffi::WebhookServerSummary> =
        crate::list_webhook_servers(h).map_err(to_js_error)?;
    serde_json::to_value(summaries).map_err(|e| {
        to_js_error(NapiError::Internal {
            message: format!("webhook server summary serialization failed: {e}"),
        })
    })
}

/// Start the background sync scheduler (Phase 6).
///
/// Spawns a dedicated OS thread that wakes every
/// `tickIntervalSecs` seconds, walks the connector instance map,
/// and dispatches [`js_sync_connector`] for every connector
/// instance whose `lastSyncedAt + syncInterval` has elapsed.
///
/// Per-instance overrides for `syncInterval` and `maxBackoff` are
/// set via [`js_configure_sync_schedule`]; instances without an
/// override use the defaults provided here.
///
/// # JS argument types
///
/// All three numeric arguments are JS `number` (not `BigInt`)
/// because realistic scheduler intervals fit in `u32` and the JS
/// `Number` representation is exact for any sub-`2^53` integer.
/// N-API marshals them through the standard `u32`/`u64` adapters
/// — values larger than `Number.MAX_SAFE_INTEGER` will round-trip
/// lossily, but no realistic scheduler config approaches that
/// bound.
///
/// # Errors
///
/// * `Unavailable` if `open_store(handle)` has not yet been called.
/// * `InvalidArgument` if any argument is `0`, or if
///   `defaultMaxBackoffSecs < defaultIntervalSecs`.
/// * `Connector` if a scheduler is already running on this handle
///   (call [`js_stop_sync_scheduler`] first).
#[napi(js_name = "startSyncScheduler")]
pub fn js_start_sync_scheduler(
    handle: BigInt,
    default_interval_secs: u32,
    default_max_backoff_secs: u32,
    tick_interval_secs: u32,
) -> Result<()> {
    let h = handle_from_bigint(&handle)?;
    crate::start_sync_scheduler(
        h,
        u64::from(default_interval_secs),
        u64::from(default_max_backoff_secs),
        u64::from(tick_interval_secs),
    )
    .map_err(to_js_error)
}

/// Stop the background sync scheduler (Phase 6).
///
/// Signals shutdown to the worker thread and synchronously joins
/// it. Idempotent — calling on a runtime with no scheduler
/// running returns `Ok(())`. In-flight `sync_connector` dispatches
/// run to completion under the substrate's three-phase locking
/// discipline.
///
/// # Errors
///
/// * `Unavailable` if `open_store(handle)` has not yet been called.
#[napi(js_name = "stopSyncScheduler")]
pub fn js_stop_sync_scheduler(handle: BigInt) -> Result<()> {
    let h = handle_from_bigint(&handle)?;
    crate::stop_sync_scheduler(h).map_err(to_js_error)
}

/// Override the scheduler's policy for a specific connector
/// instance (Phase 6). The override takes precedence over the
/// defaults supplied at [`js_start_sync_scheduler`] time.
///
/// Idempotent: a second call replaces the prior policy. Also
/// resets the instance's `consecutive_failures` counter to zero —
/// the new policy starts from a clean slate.
///
/// # Errors
///
/// * `Unavailable` if `open_store(handle)` has not yet been called.
/// * `Connector` if no scheduler is running on this handle.
/// * `InvalidArgument` if `instanceId` is not a UUID,
///   `syncIntervalSecs` is `0`, or
///   `maxBackoffSecs < syncIntervalSecs`.
#[napi(js_name = "configureSyncSchedule")]
pub fn js_configure_sync_schedule(
    handle: BigInt,
    instance_id: String,
    sync_interval_secs: u32,
    max_backoff_secs: u32,
) -> Result<()> {
    let h = handle_from_bigint(&handle)?;
    crate::configure_sync_schedule(
        h,
        instance_id,
        u64::from(sync_interval_secs),
        u64::from(max_backoff_secs),
    )
    .map_err(to_js_error)
}

/// Remove the scheduler's per-instance policy override for
/// `instanceId` (Phase 6). The instance falls back to the
/// scheduler's defaults; the accounting state is cleared so a
/// long-Failing instance gets a fresh chance.
///
/// Idempotent: clearing an instance with no override is `Ok(())`.
///
/// # Errors
///
/// * `Unavailable` if `open_store(handle)` has not yet been called.
/// * `Connector` if no scheduler is running on this handle.
/// * `InvalidArgument` if `instanceId` is not a UUID.
#[napi(js_name = "clearSyncSchedule")]
pub fn js_clear_sync_schedule(handle: BigInt, instance_id: String) -> Result<()> {
    let h = handle_from_bigint(&handle)?;
    crate::clear_sync_schedule(h, instance_id).map_err(to_js_error)
}

/// Snapshot the scheduler's diagnostic state (Phase 6). Returns a
/// `serde_json::Value` ([`ffi::SyncSchedulerStatus`]) with
/// camelCase keys so callers can destructure
/// `{ isRunning, startedAtUnix, defaultIntervalSecs,
///    defaultMaxBackoffSecs, tickIntervalSecs, policyOverrideCount,
///    totalInstanceCount, lastTickAtUnix, ticksCompleted,
///    dispatchesAttempted, dispatchesSucceeded, dispatchesFailed,
///    dispatchesSkippedInProgress }` directly.
///
/// `policyOverrideCount` reports how many connector instances have
/// a custom [`crate::configure_sync_schedule`] policy set;
/// `totalInstanceCount` reports how many connector instances the
/// scheduler is driving in total. The former is a strict subset of
/// the latter — a host UI that wants "how many connectors is the
/// scheduler syncing" should read `totalInstanceCount`.
///
/// A stopped scheduler reports `isRunning=false` and zero
/// counters; this is NOT an error. The host can call
/// [`js_start_sync_scheduler`] to begin dispatching.
///
/// # Errors
///
/// * `Unavailable` if `open_store(handle)` has not yet been called.
#[napi(js_name = "syncSchedulerStatus")]
pub fn js_sync_scheduler_status(handle: BigInt) -> Result<serde_json::Value> {
    let h = handle_from_bigint(&handle)?;
    let status = crate::sync_scheduler_status(h).map_err(to_js_error)?;
    serde_json::to_value(status).map_err(|e| {
        to_js_error(NapiError::Internal {
            message: format!("sync scheduler status serialization failed: {e}"),
        })
    })
}

/// Install a global `tracing` subscriber filtered by the supplied
/// `RUST_LOG`-syntax directive (e.g. `"ffi=debug,evidence_store=info"`).
///
/// Mirrors [`crate::try_init_tracing`]. Idempotent: a second call is
/// a no-op (the substrate guards against installing competing
/// subscribers).
///
/// Available only when the addon was built with the
/// `tracing-subscriber` feature; without it this entry point is not
/// exposed (the underlying `crate::try_init_tracing` is also
/// feature-gated).
#[cfg(feature = "tracing-subscriber")]
#[napi(js_name = "initTracing")]
pub fn js_init_tracing(directive: String) -> Result<()> {
    // `crate::try_init_tracing` is the re-export of `ffi::try_init_tracing`
    // so its error type is `FfiError`. Funnel it through the napi error
    // mapper via the `From<FfiError> for NapiError` impl so callers see the
    // same `kind`/`message`/`detail` JSON envelope every other entry point
    // produces. The Rust signature takes `String` by value (UniFFI requires
    // an owned parameter for FFI marshalling — see the `# Parameter shape`
    // section on `ffi::try_init_tracing`), so we hand `directive` over
    // directly rather than borrowing.
    crate::try_init_tracing(directive).map_err(|e| to_js_error(crate::NapiError::from(e)))
}

#[cfg(test)]
mod tests {
    //! These tests exercise the JSON-envelope error path that
    //! JavaScript callers see at runtime. They run against the
    //! `rlib` build (not the cdylib) so they don't need a live
    //! Node.js host.

    use super::*;

    fn parse_envelope(err: &JsError) -> serde_json::Value {
        // `JsError::reason` is the JSON envelope we produced in
        // `to_js_error`. The error's `Display` impl prefixes the
        // status — read the reason directly to avoid that.
        serde_json::from_str(&err.reason).expect("error reason should be JSON")
    }

    #[test]
    fn handle_from_bigint_rejects_negative() {
        let bi = BigInt {
            sign_bit: true,
            words: vec![5],
        };
        let err = handle_from_bigint(&bi).unwrap_err();
        let env = parse_envelope(&err);
        assert_eq!(env["kind"], "InvalidArgument");
    }

    #[test]
    fn handle_from_bigint_rejects_overflow() {
        // Two-word BigInt → does not fit in u64.
        let bi = BigInt {
            sign_bit: false,
            words: vec![1, 1],
        };
        let err = handle_from_bigint(&bi).unwrap_err();
        let env = parse_envelope(&err);
        assert_eq!(env["kind"], "InvalidArgument");
    }

    #[test]
    fn handle_from_bigint_accepts_valid_u64() {
        let bi = BigInt {
            sign_bit: false,
            words: vec![42],
        };
        let h = handle_from_bigint(&bi).unwrap();
        assert_eq!(h, 42);
    }

    #[test]
    fn js_init_rejects_malformed_json() {
        let err = js_init("not a json object".into()).unwrap_err();
        let env = parse_envelope(&err);
        assert_eq!(env["kind"], "InvalidConfig");
    }

    #[test]
    fn js_init_accepts_valid_config() {
        let cfg = r#"{"data_dir":"/tmp/x","log_level":"info"}"#;
        js_init(cfg.into()).expect("valid config should accept");
    }

    // ───── Phase 4.2 — N-API resolver recv-timeout validation ─────
    //
    // The full `JsClientSecretResolver::resolve` path requires a
    // live napi env to construct a `ThreadsafeFunction`, which is
    // out of reach from `cargo test -p napi_addon` (no Node host).
    // We test the timeout-validation helper instead — the rest of
    // the path is exercised in `crates/ffi/tests/ffi_integration_tests.rs`
    // through the framework's `OAuth2Client` resolution ladder.

    #[test]
    fn resolve_recv_timeout_defaults_to_5_seconds_when_unset() {
        let t = resolve_recv_timeout(None).expect("None should be accepted");
        assert_eq!(t, std::time::Duration::from_secs(5));
    }

    #[test]
    fn resolve_recv_timeout_accepts_positive_milliseconds() {
        let t = resolve_recv_timeout(Some(2_500)).expect("positive value should be accepted");
        assert_eq!(t, std::time::Duration::from_millis(2_500));
    }

    #[test]
    fn resolve_recv_timeout_accepts_one_millisecond_minimum() {
        let t = resolve_recv_timeout(Some(1)).expect("Some(1) should be accepted");
        assert_eq!(t, std::time::Duration::from_millis(1));
    }

    #[test]
    fn resolve_recv_timeout_accepts_u32_max_for_effectively_unbounded() {
        // u32::MAX ms ≈ 49 days. Hosts that want "effectively no
        // timeout" can pass this safely.
        let t = resolve_recv_timeout(Some(u32::MAX)).expect("u32::MAX should be accepted");
        assert_eq!(t, std::time::Duration::from_millis(u64::from(u32::MAX)));
    }

    #[test]
    fn resolve_recv_timeout_rejects_zero_as_silent_footgun() {
        let err = resolve_recv_timeout(Some(0)).expect_err("Some(0) must be rejected");
        // InvalidArgument-kind error (NOT Internal) — this is a
        // caller-supplied bad input, not a substrate-side encoding
        // failure. The error.rs doc on `NapiError::Internal`
        // explicitly reserves that variant for post-FFI packaging
        // bugs; routing a pre-FFI input rejection through it would
        // pollute the host's caller-error vs. substrate-bug
        // telemetry split. The message must still guide the host
        // to the correct fix (positive value or omit the argument).
        assert_eq!(
            err.kind(),
            "InvalidArgument",
            "kind() tag must match the variant; got {err:?}",
        );
        match err {
            NapiError::InvalidArgument { message } => {
                assert!(
                    message.contains("timeoutMs"),
                    "rejection message should mention the JS argument name; got {message}",
                );
                assert!(
                    message.contains("> 0"),
                    "rejection message should explain the constraint; got {message}",
                );
            }
            other => panic!("expected NapiError::InvalidArgument, got {other:?}"),
        }
    }

    #[test]
    fn js_client_secret_resolver_default_recv_timeout_is_five_seconds() {
        // Pin the documented default so the contract stays stable
        // across refactors. If we ever change the default, this
        // forces an update to the rustdoc on
        // `js_set_oauth_client_secret_resolver` too.
        assert_eq!(
            JsClientSecretResolver::DEFAULT_RECV_TIMEOUT,
            std::time::Duration::from_secs(5),
        );
    }

    // ──────────────────── JsKeyStorageResolver ────────────────────
    //
    // Same caveat as the OAuth client-secret resolver tests: the
    // full `JsKeyStorageResolver::{load_key, store_key, delete_key}`
    // path requires a live napi env to construct a
    // `ThreadsafeFunction`, which is out of reach from
    // `cargo test -p napi_addon` (no Node host). We test the
    // timeout-validation helper and the documented-default
    // constants; the end-to-end path is exercised by the
    // FFI-level resolver tests in `crates/ffi/src/runtime.rs`
    // (`open_store_with_resolver_*`).

    #[test]
    fn resolve_key_storage_recv_timeout_defaults_to_30_seconds_when_unset() {
        let t = resolve_key_storage_recv_timeout(None).expect("None should be accepted");
        assert_eq!(t, std::time::Duration::from_secs(30));
    }

    #[test]
    fn resolve_key_storage_recv_timeout_accepts_positive_milliseconds() {
        let t = resolve_key_storage_recv_timeout(Some(15_000))
            .expect("positive value should be accepted");
        assert_eq!(t, std::time::Duration::from_millis(15_000));
    }

    #[test]
    fn resolve_key_storage_recv_timeout_accepts_one_millisecond_minimum() {
        let t = resolve_key_storage_recv_timeout(Some(1)).expect("Some(1) should be accepted");
        assert_eq!(t, std::time::Duration::from_millis(1));
    }

    #[test]
    fn resolve_key_storage_recv_timeout_accepts_u32_max_for_effectively_unbounded() {
        let t =
            resolve_key_storage_recv_timeout(Some(u32::MAX)).expect("u32::MAX should be accepted");
        assert_eq!(t, std::time::Duration::from_millis(u64::from(u32::MAX)));
    }

    #[test]
    fn resolve_key_storage_recv_timeout_rejects_zero_as_silent_footgun() {
        let err = resolve_key_storage_recv_timeout(Some(0))
            .expect_err("Some(0) must be rejected — same rationale as the OAuth resolver");
        assert_eq!(
            err.kind(),
            "InvalidArgument",
            "kind() tag must match the variant; got {err:?}",
        );
        match err {
            NapiError::InvalidArgument { message } => {
                assert!(
                    message.contains("timeoutMs"),
                    "rejection message should mention the JS argument name; got {message}",
                );
                assert!(
                    message.contains("> 0"),
                    "rejection message should explain the constraint; got {message}",
                );
                assert!(
                    message.contains("30000"),
                    "rejection message should mention the documented default ms; got {message}",
                );
            }
            other => panic!("expected NapiError::InvalidArgument, got {other:?}"),
        }
    }

    #[test]
    fn js_key_storage_resolver_default_recv_timeout_is_thirty_seconds() {
        // Pin the documented default so the contract stays stable
        // across refactors. The 30 s ceiling is longer than the
        // OAuth resolver's 5 s because master-key lookups can prompt
        // the OS for biometric/password input — see the rustdoc on
        // `js_set_key_storage_resolver` for the rationale.
        assert_eq!(
            JsKeyStorageResolver::DEFAULT_RECV_TIMEOUT,
            std::time::Duration::from_secs(30),
        );
    }

    #[test]
    fn js_ingest_message_forwards_invalid_id_for_malformed_scope() {
        let req = serde_json::json!({
            "scope_id": "not-a-uuid",
            "body": "hi",
            "source": "Slack",
            "importance": "Important",
        });
        let bi = BigInt::from(RuntimeHandle::NONE.0);
        let err = js_ingest_message(bi, req).unwrap_err();
        let env = parse_envelope(&err);
        assert_eq!(env["kind"], "InvalidId");
    }

    #[test]
    fn js_query_round_trips_through_json_envelope() {
        let req = serde_json::json!({
            "scope_id": "scope",
            "query_text": "q",
            "limit": 10,
        });
        let bi = BigInt::from(RuntimeHandle::NONE.0);
        let err = js_query(bi, req).unwrap_err();
        let env = parse_envelope(&err);
        assert_eq!(env["kind"], "InvalidId");
    }

    #[test]
    fn js_list_memories_parses_filter_object() {
        let filter = serde_json::json!({ "state": "Reinforced", "pinned_only": false });
        let bi = BigInt::from(RuntimeHandle::NONE.0);
        let err = js_list_memories(bi, "scope".into(), filter).unwrap_err();
        let env = parse_envelope(&err);
        assert_eq!(env["kind"], "InvalidId");
    }

    #[test]
    fn js_list_memories_rejects_camelcase_pinned_only_typo() {
        // Companion to the FFI-level pin
        // (`crates/ffi/src/types.rs::memory_filter_rejects_camelcase_pinned_only_alias`).
        // Exercises the N-API wrapper end-to-end: a JS caller
        // shipping `{ pinnedOnly: false }` instead of
        // `{ pinned_only: false }` must come back through
        // [`to_js_error`] as an `InvalidArgument` envelope —
        // never `InvalidId` (which would imply we silently
        // defaulted past the typo and then choked downstream on
        // the bogus scope id).
        let filter = serde_json::json!({ "state": null, "pinnedOnly": false });
        let bi = BigInt::from(RuntimeHandle::NONE.0);
        let err = js_list_memories(bi, "scope".into(), filter).unwrap_err();
        let env = parse_envelope(&err);
        assert_eq!(env["kind"], "InvalidArgument");
        let msg = env["message"].as_str().expect("message is a string");
        assert!(
            msg.contains("pinnedOnly"),
            "expected the JS-facing error to name the offending key `pinnedOnly`, got {msg}"
        );
    }

    #[test]
    fn js_trigger_synthesis_parses_enum_string() {
        let bi = BigInt::from(RuntimeHandle::NONE.0);
        let err = js_trigger_synthesis(bi, "scope".into(), "ManualUserAction".into()).unwrap_err();
        let env = parse_envelope(&err);
        assert_eq!(env["kind"], "InvalidId");
    }

    #[test]
    fn js_trigger_synthesis_rejects_unknown_trigger() {
        let bi = BigInt::from(RuntimeHandle::NONE.0);
        let err = js_trigger_synthesis(bi, "scope".into(), "Bogus".into()).unwrap_err();
        let env = parse_envelope(&err);
        assert_eq!(env["kind"], "InvalidArgument");
    }

    #[test]
    fn js_generate_keypair_returns_ml_dsa_envelope() {
        let v = js_generate_keypair().expect("keypair");
        assert_eq!(v["algorithm"], "ml-dsa-65");
        assert!(v["public_key"].is_array() || v["public_key"].is_string());
    }

    #[test]
    fn js_escape_fts_query_matches_pure_rust_impl() {
        let input = r#"hello "world""#;
        let got = js_escape_fts_query(input.into());
        let want = crate::escape_fts_query(input.into());
        assert_eq!(got, want);
    }

    #[test]
    fn js_core_version_matches_cargo_pkg_version() {
        assert_eq!(js_core_version(), env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn js_health_check_without_handle_returns_bridge_only_envelope() {
        let envelope = js_health_check(None).expect("bridge probe is infallible");
        let core_version = envelope
            .get("core_version")
            .and_then(|v| v.as_str())
            .expect("core_version is a string");
        assert_eq!(core_version, env!("CARGO_PKG_VERSION"));
        let subsystems = envelope
            .get("subsystems")
            .and_then(|v| v.as_array())
            .expect("subsystems is an array");
        assert_eq!(subsystems.len(), 1);
        assert_eq!(
            subsystems[0].get("name").and_then(|v| v.as_str()),
            Some("bridge")
        );
        assert_eq!(
            subsystems[0].get("status").and_then(|v| v.as_str()),
            Some("ok")
        );
        // tracing_initialized starts false in a fresh process; the
        // rlib test build does not link `tracing-subscriber` by
        // default. If a sibling test in the same binary has already
        // installed a subscriber the flag may flip to true, so we
        // only assert the field exists and is a boolean.
        assert!(envelope
            .get("tracing_initialized")
            .is_some_and(serde_json::Value::is_boolean));
        // The metrics snapshot is wire-flat — every counter / gauge
        // is a sibling field of the snapshot object. At minimum the
        // ingest counter and the boot timestamp must exist.
        let metrics = envelope.get("metrics").expect("metrics object");
        assert!(metrics
            .get("ingest_total")
            .and_then(serde_json::Value::as_u64)
            .is_some());
        assert!(metrics
            .get("boot_unix_secs")
            .and_then(serde_json::Value::as_u64)
            .is_some_and(|v| v > 0));
    }

    #[test]
    fn js_health_check_with_zero_bigint_is_equivalent_to_none() {
        let zero = BigInt {
            sign_bit: false,
            words: vec![0],
        };
        let envelope = js_health_check(Some(zero)).expect("zero handle == bridge-only");
        let subsystems = envelope
            .get("subsystems")
            .and_then(|v| v.as_array())
            .expect("subsystems is an array");
        assert_eq!(subsystems.len(), 1);
        assert_eq!(
            subsystems[0].get("name").and_then(|v| v.as_str()),
            Some("bridge")
        );
    }

    #[test]
    fn js_health_check_with_unknown_handle_returns_unavailable_envelope_error() {
        let bogus = BigInt {
            sign_bit: false,
            words: vec![u64::MAX],
        };
        let err = js_health_check(Some(bogus)).expect_err("unknown handle");
        let env = parse_envelope(&err);
        assert_eq!(
            env.get("kind").and_then(|v| v.as_str()),
            Some("Unavailable")
        );
    }

    #[test]
    fn js_close_store_accepts_none_sentinel() {
        // The NONE sentinel (handle 0) is allowed; close_store
        // documented as a no-op for unknown handles.
        let bi = BigInt::from(RuntimeHandle::NONE.0);
        js_close_store(bi).expect("close NONE should succeed");
    }

    #[test]
    fn js_open_store_rejects_malformed_master_key() {
        // 64-char string but contains non-hex characters → InvalidArgument
        // forwarded from the FFI surface.
        let err = js_open_store(
            "/tmp/should-not-be-touched".into(),
            "ZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZ".into(),
        )
        .unwrap_err();
        let env = parse_envelope(&err);
        // FFI surface flags this as InvalidArgument (hex decode failure).
        assert!(
            env["kind"] == "InvalidArgument" || env["kind"] == "InvalidId",
            "got kind = {}",
            env["kind"]
        );
    }

    #[test]
    fn js_get_evidence_forwards_invalid_id_for_malformed_id() {
        let bi = BigInt::from(RuntimeHandle::NONE.0);
        let err = js_get_evidence(bi, "not-a-uuid".into()).unwrap_err();
        let env = parse_envelope(&err);
        assert_eq!(env["kind"], "InvalidId");
    }

    #[test]
    fn js_pin_unpin_forget_forward_invalid_id() {
        type F = fn(BigInt, String) -> Result<()>;
        for f in [js_pin as F, js_unpin as F, js_forget as F] {
            let bi = BigInt::from(RuntimeHandle::NONE.0);
            let err = f(bi, "id".into()).unwrap_err();
            let env = parse_envelope(&err);
            assert_eq!(env["kind"], "InvalidId");
        }
    }

    #[test]
    fn js_forget_scope_forwards_invalid_id_for_malformed_scope() {
        let bi = BigInt::from(RuntimeHandle::NONE.0);
        let err = js_forget_scope(bi, "not-a-uuid".into()).unwrap_err();
        let env = parse_envelope(&err);
        assert_eq!(env["kind"], "InvalidId");
    }

    #[test]
    fn js_get_user_memory_forwards_invalid_id_for_malformed_scope() {
        let bi = BigInt::from(RuntimeHandle::NONE.0);
        let err = js_get_user_memory(bi, "scope".into()).unwrap_err();
        let env = parse_envelope(&err);
        assert_eq!(env["kind"], "InvalidId");
    }

    #[test]
    fn js_run_decay_sweep_forwards_invalid_id_for_malformed_scope() {
        let bi = BigInt::from(RuntimeHandle::NONE.0);
        let err = js_run_decay_sweep(bi, "scope".into()).unwrap_err();
        let env = parse_envelope(&err);
        assert_eq!(env["kind"], "InvalidId");
    }

    #[test]
    fn js_get_channel_memory_forwards_invalid_id_for_malformed_scope() {
        let bi = BigInt::from(RuntimeHandle::NONE.0);
        let err = js_get_channel_memory(bi, "scope".into()).unwrap_err();
        let env = parse_envelope(&err);
        assert_eq!(env["kind"], "InvalidId");
    }

    #[test]
    fn js_encrypt_decrypt_forward_invalid_id_for_malformed_scope() {
        let bi = BigInt::from(RuntimeHandle::NONE.0);
        let err = js_encrypt(bi.clone(), "scope".into(), "AAEC".into()).unwrap_err();
        let env = parse_envelope(&err);
        assert_eq!(env["kind"], "InvalidId");

        let err = js_decrypt(bi, "scope".into(), "AAEC".into()).unwrap_err();
        let env = parse_envelope(&err);
        assert_eq!(env["kind"], "InvalidId");
    }

    #[test]
    fn parse_arg_rejects_invalid_json_shape() {
        // An IngestRequest with missing required fields should yield
        // an `InvalidArgument` envelope rather than panicking.
        let req = serde_json::json!({"scope_id": "x"});
        let result: Result<IngestRequest> = parse_arg(req);
        let err = result.unwrap_err();
        let env = parse_envelope(&err);
        assert_eq!(env["kind"], "InvalidArgument");
    }

    #[test]
    fn js_init_round_trip_via_json() {
        // The full JSON round-trip exercise the same code path
        // JavaScript callers see: stringified config → parse →
        // accept. A regression here would silently break the
        // Electron bootstrapper.
        let cfg = crate::types::InitConfig {
            data_dir: "/tmp/k".into(),
            log_level: "warn".into(),
        };
        let s = serde_json::to_string(&cfg).unwrap();
        js_init(s).expect("config should round-trip through json");
    }
}
