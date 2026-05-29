//! Server-side synthesis FFI entry points (Phase 7).
//!
//! Exposes four entry points to platform hosts:
//!
//! * [`configure_synthesis_engine`] — install the
//!   [`synthesis_engine::HttpManagedEndpointSynthesizer`] (plus
//!   optional scope-binding allow-list) on the runtime.
//! * [`trigger_server_synthesis`] — dispatch a domain / tenant
//!   synthesis run, applying the three-phase locking discipline
//!   (gather-locked → dispatch-unlocked → apply-locked) used by
//!   [`crate::trigger_synthesis`] and
//!   [`crate::sync_connector`](crate::connector::sync_connector).
//! * [`synthesis_status`] — look up the lifecycle status of a
//!   previously dispatched window.
//! * [`list_recent_syntheses`] — enumerate the per-scope synthesis
//!   history (newest first, capped at [`LIST_RECENT_SYNTHESES_CAP`]
//!   to bound response size).
//!
//! # Architectural contracts
//!
//! * **Three-phase locking.** The dispatch (Phase 2) runs **without**
//!   the per-handle [`crate::FfiRuntime`] mutex so concurrent FFI
//!   calls on the same handle are not blocked behind the
//!   (potentially multi-second) HTTPS call to the managed endpoint.
//!   The engine is stored as `Arc<dyn SynthesisEngine>` so Phase 1
//!   can clone the trait object out of the mutex.
//! * **Per-(scope, tier) cooldown.** A successful dispatch records
//!   `synthesis_cooldowns[(scope, tier)] = Utc::now()`. A
//!   subsequent dispatch of the *same tier* on the *same scope*
//!   within [`PER_SCOPE_COOLDOWN_SECS`] returns the most recent
//!   `Complete` window of that tier without re-running synthesis.
//!   The scheduler's auto-synthesis hook uses the same map so a
//!   host that triggers Domain synthesis manually also throttles
//!   the scheduler's next Domain attempt. Tenant cooldowns are
//!   tracked independently so a recent Domain run does NOT
//!   short-circuit a Tenant request on the same scope.
//! * **Window retention cap.** After every successful synthesis the
//!   substrate prunes completed windows beyond
//!   [`WINDOW_RETENTION_CAP_PER_SCOPE`]. Older completed windows
//!   (and their persisted [`synthesis_pipeline::SynthesisObject`]
//!   rows) are removed; in-progress / failed / pending windows are
//!   never pruned.
//! * **Output size cap.** The substrate refuses to install
//!   synthesis objects whose payload exceeds
//!   [`MAX_SYNTHESIS_OUTPUT_BYTES`] so a misbehaving endpoint
//!   cannot fill the evidence store with a single recap. Hosts see
//!   [`FfiError::Synthesis`] in that case.
//! * **Scope-binding enforcement.** When a host installs the
//!   engine with [`SynthesisEngineConfig::scope_bindings`] set, the
//!   FFI layer enforces the allow-list before dispatch (mirrors
//!   [`synthesis_engine::tee_worker::TeeWorker::assert_scope_allowed`]).
//!   An unconfigured allow-list logs a warning on every dispatch.

use std::sync::Arc;
use std::time::Duration;
#[cfg(feature = "http-client")]
use synthesis_engine::BlockingHttpClientAdapter;
#[cfg(feature = "http-client")]
use synthesis_engine::HttpManagedEndpointSynthesizer;

use chrono::Utc;
use evidence_store::{ApprovedDocumentPayloadMeta, ScopeId};
use memory_manager::ApprovedDocumentRef;
use uuid::Uuid;

use synthesis_engine::{EndpointConfig, EngineError, SynthesisEngine};
use synthesis_pipeline::{
    ApprovedDocument, ChannelOutput, DomainOutput, DomainSynthesisInput,
    HierarchyEnforcedWindowManager, SynthesisObject, SynthesisObjectType, SynthesisWindowManager,
    TenantSynthesisInput, TieredWindowHandle, WindowId, WindowScopeTier, WindowStatus,
};

use crate::error::{FfiError, FfiResult};
use crate::metrics;
use crate::runtime::{with_runtime, FfiRuntime, RuntimeHandle};
use crate::types::{
    ApprovedDocumentSummary, ScopeIdString, SynthesisEngineConfig, SynthesisStatusRecord,
    SynthesisTierKind,
};

/// Per-scope cooldown between explicit `trigger_server_synthesis`
/// calls. Matches the scheduler's auto-synthesis throttle so the
/// two paths cannot collude into a runaway synthesis loop.
pub const PER_SCOPE_COOLDOWN_SECS: i64 = 300;

/// Default rolling window size used by `trigger_server_synthesis`
/// when the host does not preconfigure window boundaries. Picked at
/// 7 days to capture a meaningful slice of activity without
/// dominating the SLM prompt.
pub const DEFAULT_WINDOW_DURATION_SECS: i64 = 7 * 24 * 60 * 60;

/// Maximum number of completed windows the substrate retains
/// per scope. Pruned after every successful synthesis.
pub const WINDOW_RETENTION_CAP_PER_SCOPE: usize = 20;

/// Hard cap on the size of a single synthesis output payload. The
/// design target is ~5 KB for domain summaries and ~10 KB for
/// tenant summaries — the 32 KiB cap allows generous headroom while
/// still bounding evidence-store growth.
pub const MAX_SYNTHESIS_OUTPUT_BYTES: usize = 32_768;

/// Maximum number of records returned by
/// [`list_recent_syntheses`]. Bounds the response size so a host
/// that opens a long-lived database does not pay an unbounded
/// serialisation cost on every status poll.
pub const LIST_RECENT_SYNTHESES_CAP: usize = 50;

/// Upper bound on the HTTP timeout the FFI accepts in
/// [`SynthesisEngineConfig::timeout_ms`]. Picked at 10 minutes
/// (600 000 ms): production synthesis dispatches are gated by the
/// per-(scope, tier) cooldown at [`PER_SCOPE_COOLDOWN_SECS`]
/// (5 min) and realistic SLM endpoints respond well under 2 min;
/// 10 min gives generous headroom for slow networks / cold-start
/// model loads while still bounding pathological values like
/// `u64::MAX` (which `Duration::from_millis` accepts but which would
/// effectively disable the request timeout and let a wedged endpoint
/// hold the dispatch thread indefinitely). Hosts that need a value
/// above this cap should re-examine the cooldown / scheduling
/// contracts before raising the bound.
pub const MAX_TIMEOUT_MS: u64 = 10 * 60 * 1_000;

/// Hard cap on the plaintext size of a single approved-document
/// payload admitted via [`admit_approved_document`].
///
/// Picked at 16 MiB. The substrate stores each payload as a single
/// AEAD row in `evidence_store::approved_document_payloads` keyed
/// by `(scope_id, document_id)`. Above this cap a host should
/// either compress / split client-side or layer a content-addressed
/// storage adapter on top — the substrate refuses oversize payloads
/// rather than silently clamping them. The same defense-in-depth
/// rationale that drives [`MAX_SYNTHESIS_OUTPUT_BYTES`] applies
/// here: a misbehaving / compromised host cannot fill the SQLCipher
/// database with a single admission call.
///
/// The cap covers the *plaintext* size — AEAD ciphertext is 16
/// bytes longer than the plaintext (AES-GCM auth tag) plus a
/// 12-byte random nonce stored alongside the ciphertext, so a
/// 16 MiB payload becomes ~16.7 MB on disk.
pub const MAX_APPROVED_DOCUMENT_BYTES: usize = 16 * 1024 * 1024;

/// Upper bound on `label` / `approver` string lengths admitted via
/// [`admit_approved_document`]. 1 KiB each is far above any
/// realistic human-readable label, and bounds the worst-case
/// `TenantMemoryObject` serialisation cost over many refs.
pub const MAX_APPROVED_DOCUMENT_METADATA_CHARS: usize = 1024;

// ─────────────────────── Public entry points ───────────────────────

/// Install the server-side synthesis engine on `handle`.
///
/// On builds with the `http-client` feature the substrate wraps the
/// supplied [`SynthesisEngineConfig`] in a
/// [`BlockingHttpClientAdapter`]-backed
/// [`HttpManagedEndpointSynthesizer`]; on minimal builds the
/// configuration is rejected with
/// [`FfiError::Unavailable`] because no production HTTP transport
/// is compiled in.
///
/// Subsequent calls overwrite the slot — there is exactly one
/// engine per runtime. Scope-binding allow-lists installed by an
/// earlier call are replaced (or cleared when the new config has
/// `scope_bindings: None`).
///
/// # Errors
///
/// * [`FfiError::Unavailable`] if compiled without the
///   `http-client` feature, if the config-supplied URL /
///   timeout values are rejected by
///   [`BlockingHttpClientAdapter::new`], or if `config.timeout_ms`
///   exceeds [`MAX_TIMEOUT_MS`].
/// * [`FfiError::InvalidId`] if any UUID in
///   `config.scope_bindings` is malformed.
#[allow(clippy::needless_pass_by_value)] // FFI: UniFFI/N-API hand owned values across the language boundary on every call.
#[uniffi::export]
pub fn configure_synthesis_engine(
    handle: RuntimeHandle,
    config: SynthesisEngineConfig,
) -> FfiResult<()> {
    metrics::instrument(metrics::inc_configure_synthesis_engine, || {
        let endpoint_config = endpoint_config_from_ffi(&config)?;
        let scope_bindings = parse_scope_bindings(config.scope_bindings.as_deref())?;
        configure_engine_impl(handle, endpoint_config, scope_bindings)
    })
}

#[cfg(feature = "http-client")]
fn configure_engine_impl(
    handle: RuntimeHandle,
    endpoint_config: EndpointConfig,
    scope_bindings: Option<Vec<Uuid>>,
) -> FfiResult<()> {
    let client =
        BlockingHttpClientAdapter::new(&endpoint_config).map_err(|e| FfiError::Unavailable {
            subsystem: format!("synthesis_engine ({e})"),
        })?;
    let synth = HttpManagedEndpointSynthesizer::new(endpoint_config, client);
    let engine: Arc<dyn SynthesisEngine> = Arc::new(synth);
    with_runtime(handle, |rt| {
        rt.synthesis_engine = Some(engine);
        rt.synthesis_scope_bindings = scope_bindings;
        tracing::info!(
            handle = handle.0,
            scope_bindings_configured = rt.synthesis_scope_bindings.is_some(),
            "configure_synthesis_engine: synthesis engine installed",
        );
        Ok(())
    })
}

#[cfg(not(feature = "http-client"))]
fn configure_engine_impl(
    _handle: RuntimeHandle,
    _endpoint_config: EndpointConfig,
    _scope_bindings: Option<Vec<Uuid>>,
) -> FfiResult<()> {
    Err(FfiError::Unavailable {
        subsystem: "synthesis_engine (built without http-client feature)".into(),
    })
}

/// Dispatch a server-side synthesis run for `scope_id` at `tier`.
///
/// Returns the UUID-string of the synthesis window the host can
/// poll via [`synthesis_status`]. On cooldown short-circuit, returns
/// the most recently completed window for the same scope.
///
/// The locking discipline (gather → dispatch → apply) matches the
/// on-device [`crate::trigger_synthesis`] path. See the module
/// docs for full details.
///
/// # Errors
///
/// * [`FfiError::InvalidId`] if `scope_id` is not a valid UUID.
/// * [`FfiError::NotFound`] if `scope_id` has been forgotten or
///   has no domain/tenant memory registered for the requested tier.
/// * [`FfiError::Unavailable`] if no engine is configured.
/// * [`FfiError::Synthesis`] if the engine surfaced an error or
///   the response payload exceeds [`MAX_SYNTHESIS_OUTPUT_BYTES`].
/// * [`FfiError::Evidence`] if persisting the resulting synthesis
///   object fails.
#[allow(clippy::needless_pass_by_value)] // FFI: UniFFI/N-API hand owned values across the language boundary on every call.
#[uniffi::export]
pub fn trigger_server_synthesis(
    handle: RuntimeHandle,
    scope_id: String,
    tier: SynthesisTierKind,
) -> FfiResult<String> {
    metrics::instrument(metrics::inc_trigger_server_synthesis, || {
        let scope = crate::parse_scope_id(&scope_id)?;
        tracing::info!(
            scope = %scope.as_uuid(),
            tier = tier.as_str(),
            "trigger_server_synthesis: dispatching",
        );
        dispatch_server_synthesis(handle, scope, tier).map_err(|err| {
            tracing::warn!(
                scope = %scope.as_uuid(),
                tier = tier.as_str(),
                error = ?err,
                "trigger_server_synthesis: failed",
            );
            err
        })
    })
}

/// Look up the lifecycle status of `synthesis_id`.
///
/// # Errors
///
/// * [`FfiError::InvalidId`] if `synthesis_id` is not a valid
///   UUID.
/// * [`FfiError::NotFound`] if the substrate does not know of any
///   window with that id.
#[allow(clippy::needless_pass_by_value)] // FFI: UniFFI/N-API hand owned values across the language boundary on every call.
#[uniffi::export]
pub fn synthesis_status(
    handle: RuntimeHandle,
    synthesis_id: String,
) -> FfiResult<SynthesisStatusRecord> {
    metrics::instrument(metrics::inc_synthesis_status, || {
        let window_id = parse_window_id(&synthesis_id)?;
        with_runtime(handle, |rt| {
            let window = rt
                .synthesis_windows
                .get(window_id)
                .ok_or_else(|| FfiError::NotFound {
                    kind: "synthesis_window".into(),
                    id: synthesis_id.clone(),
                })?;
            Ok(window_to_record(window, rt))
        })
    })
}

/// Enumerate recent synthesis windows for `scope_id`, newest
/// first, capped at [`LIST_RECENT_SYNTHESES_CAP`].
///
/// Returns an empty vector for a scope with no recorded synthesis
/// history (including the forgotten / unknown / never-touched
/// cases) — [`synthesis_status`] is the entry point hosts should
/// use to distinguish unknown ids.
///
/// # Errors
///
/// * [`FfiError::InvalidId`] if `scope_id` is not a valid UUID.
#[allow(clippy::needless_pass_by_value)] // FFI: UniFFI/N-API hand owned values across the language boundary on every call.
#[uniffi::export]
pub fn list_recent_syntheses(
    handle: RuntimeHandle,
    scope_id: String,
) -> FfiResult<Vec<SynthesisStatusRecord>> {
    metrics::instrument(metrics::inc_list_recent_syntheses, || {
        let scope = crate::parse_scope_id(&scope_id)?;
        with_runtime(handle, |rt| {
            if rt.is_scope_forgotten(scope) {
                return Ok(Vec::new());
            }
            let mut records: Vec<SynthesisStatusRecord> = rt
                .synthesis_windows
                .windows_for(scope)
                .into_iter()
                .map(|w| window_to_record(w, rt))
                .collect();
            // Newest first by window_end.
            records.sort_by_key(|r| std::cmp::Reverse(r.window_end_unix));
            records.truncate(LIST_RECENT_SYNTHESES_CAP);
            Ok(records)
        })
    })
}

/// Admit an approved official document onto the tenant memory at
/// `scope_id` (Phase 8).
///
/// The substrate stores the AEAD-encrypted payload bytes in
/// `evidence_store::approved_document_payloads` under the per-scope
/// DEK, mints a fresh [`ApprovedDocumentRef`] (with the freshly
/// generated UUID, supplied `label` / `approver`, and `Utc::now()`),
/// admits the ref onto the tenant memory, and flushes the tenant
/// memory blob. A subsequent
/// [`trigger_server_synthesis`] at `tier = Tenant` rehydrates the
/// payload, joins it with the ref by id, and ships the resulting
/// [`ApprovedDocument`] bundle to the configured engine.
///
/// Re-admitting a document is a *new ref* (new UUID, new payload
/// row). Hosts that want to overwrite an existing document should
/// [`revoke_approved_document`] first, then call this.
///
/// # Validation
///
/// * `label` and `approver` must be non-empty and at most
///   [`MAX_APPROVED_DOCUMENT_METADATA_CHARS`] bytes. Empty values
///   are rejected with [`FfiError::Memory`].
/// * `payload` must be non-empty and at most
///   [`MAX_APPROVED_DOCUMENT_BYTES`]. Oversize is rejected with
///   [`FfiError::Memory`] whose message names both the supplied
///   size and the cap so the host can size its admission flow
///   correctly.
///
/// # Errors
///
/// * [`FfiError::Unavailable`] if [`crate::open_store`] has not
///   been called.
/// * [`FfiError::InvalidId`] if `scope_id` is not a valid UUID.
/// * [`FfiError::NotFound`] if `scope_id` has been
///   cryptographically forgotten via [`crate::forget_scope`].
/// * [`FfiError::Memory`] for the validation cases above, or if
///   the tenant memory cannot be serialised.
/// * [`FfiError::Evidence`] if the underlying evidence store fails
///   to persist the payload row or the tenant memory blob.
#[allow(clippy::needless_pass_by_value)] // FFI: UniFFI/N-API hand owned values across the language boundary on every call.
#[uniffi::export]
pub fn admit_approved_document(
    handle: RuntimeHandle,
    scope_id: ScopeIdString,
    label: String,
    approver: String,
    payload: Vec<u8>,
) -> FfiResult<ApprovedDocumentSummary> {
    metrics::instrument(metrics::inc_admit_approved_document, || {
        let scope = crate::parse_scope_id(&scope_id)?;
        validate_approved_document_metadata("label", &label)?;
        validate_approved_document_metadata("approver", &approver)?;
        if payload.is_empty() {
            return Err(FfiError::Memory {
                message: "admit_approved_document: payload must be non-empty".into(),
            });
        }
        if payload.len() > MAX_APPROVED_DOCUMENT_BYTES {
            return Err(FfiError::Memory {
                message: format!(
                    "admit_approved_document: payload size {} bytes exceeds the {} byte cap \
                     ({MAX_APPROVED_DOCUMENT_BYTES_MIB} MiB); compress or split client-side \
                     before admission",
                    payload.len(),
                    MAX_APPROVED_DOCUMENT_BYTES,
                    MAX_APPROVED_DOCUMENT_BYTES_MIB = MAX_APPROVED_DOCUMENT_BYTES / (1024 * 1024),
                ),
            });
        }
        let content_hash = crypto::content_hash(&payload);
        let payload_bytes = payload.len() as u64;
        let reference = ApprovedDocumentRef::new(label, approver);
        let summary = ApprovedDocumentSummary {
            id: reference.id.to_string(),
            scope_id: scope_id.clone(),
            label: reference.label.clone(),
            approver: reference.approver.clone(),
            approved_at_ms: reference.approved_at.timestamp_millis(),
            payload_bytes,
            content_hash_hex: encode_content_hash_hex(&content_hash),
        };
        let doc_id = reference.id;
        with_runtime(handle, |rt| {
            if rt.is_scope_forgotten(scope) {
                return Err(FfiError::NotFound {
                    kind: "scope".into(),
                    id: scope_id.clone(),
                });
            }
            rt.ensure_scope_registered(scope)?;
            // Persist-first invariant: write the AEAD payload row to
            // SQLCipher BEFORE mutating the tenant-memory map so a
            // crash between the two steps leaves the substrate in
            // the pre-admission state (the orphan payload row is
            // harmless — `list_approved_documents` joins on the
            // tenant-memory ref list, so an orphan row is filtered
            // out; the next `forget_scope` on this scope also purges
            // it).
            rt.store()
                .save_approved_document_payload(scope, doc_id, &payload, &content_hash)
                .map_err(|e| FfiError::Evidence {
                    message: format!("save_approved_document_payload failed: {e}"),
                })?;
            let mut tmo = rt
                .tenant_memory(scope)
                .cloned()
                .unwrap_or_else(|| memory_manager::TenantMemoryObject::new(scope));
            tmo.admit_approved_document(reference);
            rt.save_tenant_memory(scope, tmo)?;
            tracing::info!(
                scope = %scope.as_uuid(),
                document_id = %doc_id,
                payload_bytes,
                "admit_approved_document: persisted payload + tenant ref",
            );
            Ok(summary.clone())
        })
    })
}

/// Revoke a previously admitted approved document (Phase 8).
///
/// Removes both the tenant-memory ref and the persisted payload
/// row. The tenant memory blob is re-flushed so the revocation
/// survives a restart. Tenant synthesis windows opened *after* this
/// call will not see the revoked document; in-flight windows are
/// unaffected (the Phase-1 gather snapshot captured the input
/// before the revocation).
///
/// Idempotent on a fully revoked document: a second call returns
/// [`FfiError::NotFound`] because the ref is gone from tenant
/// memory.
///
/// # Errors
///
/// * [`FfiError::Unavailable`] if [`crate::open_store`] has not
///   been called.
/// * [`FfiError::InvalidId`] if `scope_id` or `document_id` is not
///   a valid UUID.
/// * [`FfiError::NotFound`] if the scope has been forgotten, no
///   tenant memory exists for the scope, or the document id is
///   not registered on the tenant memory.
/// * [`FfiError::Evidence`] if the underlying evidence store fails
///   to delete the payload row or persist the updated tenant
///   memory.
#[allow(clippy::needless_pass_by_value)] // FFI: UniFFI/N-API hand owned values across the language boundary on every call.
#[uniffi::export]
pub fn revoke_approved_document(
    handle: RuntimeHandle,
    scope_id: ScopeIdString,
    document_id: String,
) -> FfiResult<()> {
    metrics::instrument(metrics::inc_revoke_approved_document, || {
        let scope = crate::parse_scope_id(&scope_id)?;
        let doc_uuid = Uuid::parse_str(&document_id).map_err(|e| FfiError::InvalidId {
            message: format!("document_id: {e}"),
        })?;
        with_runtime(handle, |rt| {
            if rt.is_scope_forgotten(scope) {
                return Err(FfiError::NotFound {
                    kind: "scope".into(),
                    id: scope_id.clone(),
                });
            }
            let mut tmo = rt
                .tenant_memory(scope)
                .cloned()
                .ok_or_else(|| FfiError::NotFound {
                    kind: "tenant_memory".into(),
                    id: scope_id.clone(),
                })?;
            tmo.revoke_approved_document(doc_uuid)
                .map_err(|_| FfiError::NotFound {
                    kind: "approved_document".into(),
                    id: document_id.clone(),
                })?;
            // Persist-first invariant (mirror of `admit_approved_document`):
            // flush the updated tenant memory FIRST so a crash before the
            // payload-row delete leaves the host-observable contract
            // consistent — the ref is gone, the orphan payload row is
            // unreachable through the tenant-memory join and will be
            // purged on the next `forget_scope` for this scope.
            rt.save_tenant_memory(scope, tmo)?;
            let deleted = rt
                .store()
                .delete_approved_document_payload(scope, doc_uuid)
                .map_err(|e| FfiError::Evidence {
                    message: format!("delete_approved_document_payload failed: {e}"),
                })?;
            tracing::info!(
                scope = %scope.as_uuid(),
                document_id = %doc_uuid,
                rows_deleted = deleted,
                "revoke_approved_document: removed tenant ref and payload row",
            );
            Ok(())
        })
    })
}

/// List approved-document refs admitted to the tenant memory at
/// `scope_id`, joined with each ref's persisted payload metadata
/// (Phase 8).
///
/// The order matches `TenantMemoryObject.approved_documents`
/// insertion order. Returns an empty vector for a forgotten scope
/// or a scope with no tenant memory (no `Err`) so callers can
/// treat both cases the same as "nothing admitted".
///
/// Refs without a persisted payload row (e.g. legacy refs created
/// before the Phase 8 admission path, or a payload row that was
/// purged out-of-band) are still surfaced with
/// `payload_bytes = 0` and `content_hash_hex = ""` so the host can
/// detect and act on the gap rather than silently dropping the
/// row.
///
/// # Errors
///
/// * [`FfiError::Unavailable`] if [`crate::open_store`] has not
///   been called.
/// * [`FfiError::InvalidId`] if `scope_id` is not a valid UUID.
/// * [`FfiError::Evidence`] if the underlying metadata query
///   fails (does NOT decrypt any ciphertext).
#[allow(clippy::needless_pass_by_value)] // FFI: UniFFI/N-API hand owned values across the language boundary on every call.
#[uniffi::export]
pub fn list_approved_documents(
    handle: RuntimeHandle,
    scope_id: ScopeIdString,
) -> FfiResult<Vec<ApprovedDocumentSummary>> {
    metrics::instrument(metrics::inc_list_approved_documents, || {
        let scope = crate::parse_scope_id(&scope_id)?;
        with_runtime(handle, |rt| {
            if rt.is_scope_forgotten(scope) {
                return Ok(Vec::new());
            }
            let Some(tmo) = rt.tenant_memory(scope) else {
                return Ok(Vec::new());
            };
            let meta_rows = rt
                .store()
                .list_approved_document_payload_meta_for_scope(scope)
                .map_err(|e| FfiError::Evidence {
                    message: format!("list_approved_document_payload_meta_for_scope failed: {e}"),
                })?;
            let meta_by_id: std::collections::HashMap<Uuid, ApprovedDocumentPayloadMeta> =
                meta_rows.into_iter().map(|m| (m.document_id, m)).collect();
            let summaries: Vec<ApprovedDocumentSummary> = tmo
                .approved_documents
                .iter()
                .map(|r| {
                    let (payload_bytes, content_hash_hex) = match meta_by_id.get(&r.id) {
                        Some(meta) => {
                            (meta.size_bytes, encode_content_hash_hex(&meta.content_hash))
                        }
                        None => (0u64, String::new()),
                    };
                    ApprovedDocumentSummary {
                        id: r.id.to_string(),
                        scope_id: scope_id.clone(),
                        label: r.label.clone(),
                        approver: r.approver.clone(),
                        approved_at_ms: r.approved_at.timestamp_millis(),
                        payload_bytes,
                        content_hash_hex,
                    }
                })
                .collect();
            Ok(summaries)
        })
    })
}

// ─────────────────────── Implementation details ───────────────────────

/// Snapshot captured while the runtime mutex is held (Phase 1) and
/// then consumed during the unlocked dispatch (Phase 2) and the
/// post-dispatch apply (Phase 3).
///
/// `windows_clone` is a deep copy of the live
/// [`SynthesisWindowManager`] taken under the mutex; the engine
/// validates `handle.window_id` against it during Phase 2 (unlocked).
/// Phase 3 replays the Pending → InProgress → Complete transitions on
/// the live manager so the substrate's persisted state ends up
/// identical to what the engine observed.
struct DomainDispatchPlan {
    engine: Arc<dyn SynthesisEngine>,
    window_handle: TieredWindowHandle,
    input: DomainSynthesisInput,
    windows_clone: SynthesisWindowManager,
}

struct TenantDispatchPlan {
    engine: Arc<dyn SynthesisEngine>,
    window_handle: TieredWindowHandle,
    input: TenantSynthesisInput,
    windows_clone: SynthesisWindowManager,
}

enum DispatchPlan {
    Domain(DomainDispatchPlan),
    Tenant(TenantDispatchPlan),
    /// Cooldown short-circuit: return the supplied window id
    /// without dispatching.
    Cooldown(WindowId),
}

fn dispatch_server_synthesis(
    handle: RuntimeHandle,
    scope: ScopeId,
    tier: SynthesisTierKind,
) -> FfiResult<String> {
    // ─────────────── Phase 1: gather (locked) ───────────────
    //
    // Validate the scope, check the cooldown window, gather the
    // hierarchy input (channel outputs for domain, domain outputs
    // for tenant), open a `Pending` window, clone the engine `Arc`
    // out of the mutex, and return the resulting plan. The
    // synthesis window stays in `Pending` after Phase 1; Phase 2
    // (unlocked) issues the HTTP call; Phase 3 (locked) marks the
    // window `Complete` / `Failed` based on the dispatch outcome.
    let plan = with_runtime(handle, |rt| build_dispatch_plan(rt, scope, tier))?;

    // Cooldown short-circuit returns the cached window id without
    // entering Phase 2.
    let (engine, window_handle, dispatch_result) = match plan {
        DispatchPlan::Cooldown(window_id) => {
            tracing::info!(
                scope = %scope.as_uuid(),
                window = %window_id.as_uuid(),
                cooldown_secs = PER_SCOPE_COOLDOWN_SECS,
                "trigger_server_synthesis: returning cached window (cooldown)",
            );
            return Ok(window_id.as_uuid().to_string());
        }
        DispatchPlan::Domain(plan) => {
            let DomainDispatchPlan {
                engine,
                window_handle,
                input,
                mut windows_clone,
            } = plan;
            // ─────────── Phase 2: dispatch (UNLOCKED) ───────────
            //
            // The engine validates `handle.window_id` against
            // `windows_clone` (the snapshot we took under the
            // mutex) and transitions its window state. We do not
            // persist `windows_clone` — Phase 3 replays the
            // Pending → InProgress → Complete transitions on the
            // live manager.
            let outcome = engine.synthesize_domain(&mut windows_clone, window_handle, input);
            (engine, window_handle, outcome.map(|r| r.object))
        }
        DispatchPlan::Tenant(plan) => {
            let TenantDispatchPlan {
                engine,
                window_handle,
                input,
                mut windows_clone,
            } = plan;
            let outcome = engine.synthesize_tenant(&mut windows_clone, window_handle, input);
            (engine, window_handle, outcome.map(|r| r.object))
        }
    };
    // `engine` is dropped here — the Arc was only needed for the
    // unlocked dispatch and the runtime still owns its own clone.
    drop(engine);

    // ─────────────── Phase 3: apply (locked) ───────────────
    apply_dispatch_outcome(handle, scope, tier, window_handle, dispatch_result)
}

/// Phase 1 body. Holds the runtime mutex.
fn build_dispatch_plan(
    rt: &mut FfiRuntime,
    scope: ScopeId,
    tier: SynthesisTierKind,
) -> FfiResult<DispatchPlan> {
    if rt.is_scope_forgotten(scope) {
        return Err(FfiError::NotFound {
            kind: "scope".into(),
            id: scope.as_uuid().to_string(),
        });
    }

    // Engine slot — must be installed via `configure_synthesis_engine`.
    let engine = rt
        .synthesis_engine
        .as_ref()
        .ok_or_else(|| FfiError::Unavailable {
            subsystem: "synthesis_engine".into(),
        })?
        .clone();

    // Scope-binding allow-list — mirrors `TeeWorker::assert_scope_allowed`.
    enforce_scope_binding(rt, scope)?;

    // Cooldown check. If the (scope, tier) pair was synthesised
    // within the last `PER_SCOPE_COOLDOWN_SECS` seconds, return
    // the most recent `Complete` window OF THE REQUESTED TIER
    // without re-dispatching. The map is keyed by `(scope, tier)`
    // — not just `scope` — so a recent Domain completion does NOT
    // short-circuit a Tenant request on the same scope (and vice
    // versa). See `synthesis_cooldowns` field docs in `runtime.rs`
    // for the architectural rationale.
    if let Some(last_completed) = rt.synthesis_cooldowns.get(&(scope, tier)).copied() {
        let elapsed = Utc::now().signed_duration_since(last_completed);
        if elapsed < chrono::Duration::seconds(PER_SCOPE_COOLDOWN_SECS) {
            if let Some(recent) = newest_complete_window(rt, scope, tier) {
                return Ok(DispatchPlan::Cooldown(recent));
            }
            // Cooldown stamp without a matching-tier `Complete`
            // window is a bookkeeping bug rather than a host-
            // visible failure — fall through and dispatch a fresh
            // run.
            tracing::warn!(
                scope = %scope.as_uuid(),
                tier = tier.as_str(),
                "trigger_server_synthesis: cooldown stamp present but no matching-tier \
                 Complete window; dispatching fresh run",
            );
        }
    }

    let now = Utc::now();
    let window_start = now - chrono::Duration::seconds(DEFAULT_WINDOW_DURATION_SECS);
    let window_end = now;

    match tier {
        SynthesisTierKind::Domain => {
            let domain = rt.domain_memory(scope).ok_or_else(|| FfiError::NotFound {
                kind: "domain_memory".into(),
                id: scope.as_uuid().to_string(),
            })?;
            let channel_outputs = gather_channel_outputs(rt, domain);
            let input = DomainSynthesisInput::new(domain, channel_outputs).map_err(|e| {
                FfiError::Synthesis {
                    message: format!("domain input rejected: {e}"),
                }
            })?;
            let window_handle = rt
                .synthesis_windows
                .open_tiered_window(scope, WindowScopeTier::Domain, window_start, window_end)
                .map_err(|e| FfiError::Synthesis {
                    message: format!("open_tiered_window failed: {e}"),
                })?;
            // Persist the freshly opened window so its `Pending`
            // status survives a crash between Phase 1 and Phase 3.
            rt.flush_synthesis_windows()?;
            // Take a snapshot of the live manager AFTER the open so
            // the cloned manager sees the new window in `Pending`.
            // The unlocked Phase 2 mutates this clone — Phase 3
            // replays the transitions on the live manager.
            let windows_clone = rt.synthesis_windows.clone();
            Ok(DispatchPlan::Domain(DomainDispatchPlan {
                engine,
                window_handle,
                input,
                windows_clone,
            }))
        }
        SynthesisTierKind::Tenant => {
            let tenant = rt.tenant_memory(scope).ok_or_else(|| FfiError::NotFound {
                kind: "tenant_memory".into(),
                id: scope.as_uuid().to_string(),
            })?;
            let domain_outputs = gather_domain_outputs(rt, tenant);
            // Phase 8: materialise approved-document payloads from
            // the evidence store for every ref admitted onto the
            // tenant memory. This runs under the gather lock so the
            // payload bundle is a consistent point-in-time view of
            // both the ref list and the persisted ciphertext; a
            // concurrent `admit_approved_document` /
            // `revoke_approved_document` on the same scope must wait
            // for the runtime mutex before mutating either side.
            //
            // A ref without a corresponding payload row (e.g. a host
            // that admitted a ref via the legacy pre-Phase-8 path,
            // or a payload row that was purged out-of-band) is
            // skipped with a `warn!` so the gap is observable on the
            // dispatch path rather than silently feeding an empty
            // bundle to the SLM. The synthesis run still proceeds —
            // the host may have registered other documents that DO
            // have payloads.
            let mut approved_documents: Vec<ApprovedDocument> = Vec::new();
            let mut missing_payloads: usize = 0;
            for r in &tenant.approved_documents {
                match rt.store().load_approved_document_payload(scope, r.id) {
                    Ok(Some(payload)) => {
                        approved_documents.push(ApprovedDocument::new(r.clone(), payload));
                    }
                    Ok(None) => {
                        missing_payloads += 1;
                        tracing::warn!(
                            scope = %scope.as_uuid(),
                            document_id = %r.id,
                            label = %r.label,
                            "trigger_server_synthesis(tenant): approved-document ref has no \
                             persisted payload; skipping. Re-admit the document via \
                             `admit_approved_document` to attach a payload, or call \
                             `revoke_approved_document` to drop the orphan ref.",
                        );
                    }
                    Err(e) => {
                        return Err(FfiError::Evidence {
                            message: format!(
                                "load_approved_document_payload failed for document {}: {e}",
                                r.id
                            ),
                        });
                    }
                }
            }
            if missing_payloads > 0 {
                tracing::warn!(
                    scope = %scope.as_uuid(),
                    refs_total = tenant.approved_documents.len(),
                    payloads_attached = approved_documents.len(),
                    missing_payloads,
                    "trigger_server_synthesis(tenant): dispatching with partial \
                     approved-documents bundle",
                );
            }
            let input = TenantSynthesisInput::new(tenant, domain_outputs, approved_documents)
                .map_err(|e| FfiError::Synthesis {
                    message: format!("tenant input rejected: {e}"),
                })?;
            let window_handle = rt
                .synthesis_windows
                .open_tiered_window(scope, WindowScopeTier::Tenant, window_start, window_end)
                .map_err(|e| FfiError::Synthesis {
                    message: format!("open_tiered_window failed: {e}"),
                })?;
            rt.flush_synthesis_windows()?;
            let windows_clone = rt.synthesis_windows.clone();
            Ok(DispatchPlan::Tenant(TenantDispatchPlan {
                engine,
                window_handle,
                input,
                windows_clone,
            }))
        }
    }
}

/// Phase 3 body. Holds the runtime mutex.
fn apply_dispatch_outcome(
    handle: RuntimeHandle,
    scope: ScopeId,
    tier: SynthesisTierKind,
    window_handle: TieredWindowHandle,
    dispatch_result: std::result::Result<SynthesisObject, EngineError>,
) -> FfiResult<String> {
    with_runtime(handle, |rt| {
        // TOCTOU defence: another thread may have called
        // `forget_scope` during the unlocked phase. The window
        // entry we hold a handle to is gone from the manager in
        // that case (the forgetting path drops every per-scope
        // window) so there is no state to apply — surface
        // `Unavailable` and discard the recap.
        if rt.is_scope_forgotten(scope) {
            tracing::warn!(
                scope = %scope.as_uuid(),
                window = %window_handle.window_id.as_uuid(),
                "trigger_server_synthesis: scope forgotten during dispatch; discarding recap",
            );
            return Err(FfiError::Unavailable {
                subsystem: "scope_forgotten".into(),
            });
        }
        if rt.synthesis_windows.get(window_handle.window_id).is_none() {
            // Same shape as the forgotten-scope race but with the
            // narrower per-window mutation (a host that called
            // `remove_windows_for_scope` or similar between
            // phases).
            tracing::warn!(
                scope = %scope.as_uuid(),
                window = %window_handle.window_id.as_uuid(),
                "trigger_server_synthesis: window vanished during dispatch; discarding recap",
            );
            return Err(FfiError::Unavailable {
                subsystem: "synthesis_window".into(),
            });
        }

        match dispatch_result {
            Err(err) => {
                // Best effort: mark the window failed so retries
                // and diagnostic enumeration can see the outcome.
                fail_window_on_live_manager(rt, window_handle.window_id, "dispatch_error");
                if let Err(e) = rt.flush_synthesis_windows() {
                    tracing::warn!(
                        error = ?e,
                        "post-failure flush_synthesis_windows failed",
                    );
                }
                Err(FfiError::Synthesis {
                    message: format!("server synthesis failed: {err}"),
                })
            }
            Ok(object) => {
                if object.payload.len() > MAX_SYNTHESIS_OUTPUT_BYTES {
                    fail_window_on_live_manager(rt, window_handle.window_id, "oversize_output");
                    let _ = rt.flush_synthesis_windows();
                    return Err(FfiError::Synthesis {
                        message: format!(
                            "synthesis output exceeded {} bytes (got {})",
                            MAX_SYNTHESIS_OUTPUT_BYTES,
                            object.payload.len(),
                        ),
                    });
                }
                // Cross-check that the engine emitted the object
                // for the same window/scope we authorised. Defence
                // in depth against a malicious / buggy engine.
                if object.scope_id != scope || object.window_id != window_handle.window_id {
                    fail_window_on_live_manager(
                        rt,
                        window_handle.window_id,
                        "scope_window_mismatch",
                    );
                    let _ = rt.flush_synthesis_windows();
                    return Err(FfiError::Synthesis {
                        message: format!(
                            "engine returned object for scope/window ({} / {}) but dispatch \
                             was for ({} / {})",
                            object.scope_id.as_uuid(),
                            object.window_id.as_uuid(),
                            scope.as_uuid(),
                            window_handle.window_id.as_uuid(),
                        ),
                    });
                }
                // Type contract — domain dispatches must emit
                // DomainSummary, tenant dispatches TenantSummary.
                let expected = match tier {
                    SynthesisTierKind::Domain => SynthesisObjectType::DomainSummary,
                    SynthesisTierKind::Tenant => SynthesisObjectType::TenantSummary,
                };
                if object.object_type != expected {
                    fail_window_on_live_manager(
                        rt,
                        window_handle.window_id,
                        "object_type_mismatch",
                    );
                    let _ = rt.flush_synthesis_windows();
                    return Err(FfiError::Synthesis {
                        message: format!(
                            "engine returned {} object but {} synthesis was requested",
                            object.object_type.as_str(),
                            tier.as_str(),
                        ),
                    });
                }
                // Replay the in-progress → complete transition on
                // the real window manager. Both transitions can
                // refuse if the host called `forget_scope` and
                // recreated state mid-flight — surface as
                // `Synthesis` rather than panicking.
                rt.synthesis_windows
                    .mark_in_progress(window_handle.window_id)
                    .map_err(|e| FfiError::Synthesis {
                        message: format!("mark_in_progress failed: {e}"),
                    })?;
                rt.synthesis_windows
                    .mark_complete(window_handle.window_id)
                    .map_err(|e| FfiError::Synthesis {
                        message: format!("mark_complete failed: {e}"),
                    })?;
                let window_id_str = object.window_id.as_uuid().to_string();
                let window_uuid = object.window_id.as_uuid();
                // Copy the payload text into the memory_manager
                // recap field before we move the object into the
                // store. Payload is UTF-8 enforced upstream by the
                // synthesizer; we fall back to lossy decode here
                // so a malformed adapter can never wedge the apply
                // phase.
                let recap_text = String::from_utf8(object.payload.clone())
                    .unwrap_or_else(|err| String::from_utf8_lossy(err.as_bytes()).into_owned());
                let object_tier = tier;
                // Persist the synthesis object — install into the
                // in-memory map and the encrypted evidence store.
                rt.save_synthesis_object(scope, object)?;
                // Mirror the recap into the domain / tenant memory
                // object so the next synthesis run (and any host
                // consuming the memory object directly) reflects
                // the latest output. We persist the updated memory
                // object before flushing the window manager so a
                // crash between the two leaves the windows
                // referencing the old recap rather than a partial
                // one.
                match object_tier {
                    SynthesisTierKind::Domain => {
                        let domain = rt.domain_memory_mut(scope);
                        domain.update_recap(recap_text, Some(window_uuid));
                        let domain_clone = domain.clone();
                        rt.save_domain_memory(scope, domain_clone)?;
                    }
                    SynthesisTierKind::Tenant => {
                        let tenant = rt.tenant_memory_mut(scope);
                        tenant.update_summary(recap_text, Some(window_uuid));
                        let tenant_clone = tenant.clone();
                        rt.save_tenant_memory(scope, tenant_clone)?;
                    }
                }
                // Persist window manager state (status transitions).
                rt.flush_synthesis_windows()?;
                // Cooldown stamp — keyed by `(scope, tier)` so
                // Domain and Tenant syntheses on the same scope
                // track their throttle clocks independently.
                rt.synthesis_cooldowns
                    .insert((scope, object_tier), Utc::now());
                // Retention prune. We don't surface the pruned
                // ids; the caller only cares about the new window.
                let pruned = rt.prune_completed_windows(scope, WINDOW_RETENTION_CAP_PER_SCOPE);
                if !pruned.is_empty() {
                    // Pruning mutated the manager — flush again.
                    if let Err(e) = rt.flush_synthesis_windows() {
                        tracing::warn!(
                            error = ?e,
                            "post-prune flush_synthesis_windows failed",
                        );
                    }
                    // Rewrite the per-scope `synthesis_object`
                    // blob from the post-prune in-memory state.
                    // Without this, a crash before the next
                    // successful synthesis on the same scope
                    // would rehydrate the pruned objects from
                    // disk and surface them as orphans (their
                    // `window_id` no longer maps to a tracked
                    // window because the window manager flush
                    // above already reflects the pruned set).
                    if let Err(e) = rt.flush_synthesis_objects(scope) {
                        tracing::warn!(
                            error = ?e,
                            scope = %scope.as_uuid(),
                            "post-prune flush_synthesis_objects failed; on-disk \
                             synthesis-object blob still references pruned ids and may \
                             rehydrate orphans on next open_store",
                        );
                    }
                    tracing::debug!(
                        scope = %scope.as_uuid(),
                        pruned = pruned.len(),
                        "trigger_server_synthesis: pruned completed windows beyond retention cap",
                    );
                }
                Ok(window_id_str)
            }
        }
    })
}

fn enforce_scope_binding(rt: &FfiRuntime, scope: ScopeId) -> FfiResult<()> {
    match rt.synthesis_scope_bindings.as_deref() {
        None => {
            tracing::warn!(
                scope = %scope.as_uuid(),
                "trigger_server_synthesis: no scope-binding allow-list configured; \
                 production deployments SHOULD enable scope_bindings or wrap the engine \
                 in a TeeWorker",
            );
            Ok(())
        }
        Some(bindings) => {
            if bindings.iter().any(|u| *u == scope.as_uuid()) {
                Ok(())
            } else {
                Err(FfiError::Unavailable {
                    subsystem: format!(
                        "synthesis_engine: scope {} not in configured scope_bindings",
                        scope.as_uuid()
                    ),
                })
            }
        }
    }
}

/// Collect [`ChannelOutput`]s for every channel scope registered
/// on `domain`. Each channel's most recent `ChannelRecap`
/// synthesis object (looked up by scope) is folded into the
/// resulting list. Channels with no recorded recap object are
/// skipped silently — domain synthesis is best-effort across the
/// registered set.
fn gather_channel_outputs(
    rt: &FfiRuntime,
    domain: &memory_manager::DomainMemoryObject,
) -> Vec<ChannelOutput> {
    let mut outputs = Vec::with_capacity(domain.channel_scopes.len());
    for channel_scope in &domain.channel_scopes {
        if let Some(object) = newest_channel_recap_for_scope(rt, *channel_scope) {
            match ChannelOutput::from_channel_object(object) {
                Ok(o) => outputs.push(o),
                Err(e) => {
                    tracing::warn!(
                        channel = %channel_scope.as_uuid(),
                        error = ?e,
                        "skipping channel output that failed hierarchy validation",
                    );
                }
            }
        } else if let Some(cmo) = rt.channel_memories.get(channel_scope) {
            // No persisted ChannelRecap synthesis object yet — synthesise a
            // ChannelRecap object on the fly from the latest recap text on
            // the channel memory. This keeps the domain-synthesis pipeline
            // alive on builds where channel-tier synthesis publishes its
            // recap text into `ChannelMemoryObject.recap` but does not
            // persist a SynthesisObject row.
            if cmo.recap.is_empty() {
                continue;
            }
            let synthesised = SynthesisObject::new(
                *channel_scope,
                WindowId::new_v4(),
                SynthesisObjectType::ChannelRecap,
                cmo.recap.as_bytes().to_vec(),
                Uuid::nil(),
            );
            match ChannelOutput::from_channel_object(synthesised) {
                Ok(o) => outputs.push(o),
                Err(e) => {
                    tracing::warn!(
                        channel = %channel_scope.as_uuid(),
                        error = ?e,
                        "synthesised ChannelRecap rejected by hierarchy validator",
                    );
                }
            }
        }
    }
    outputs
}

/// Collect [`DomainOutput`]s for every domain scope registered
/// on `tenant`. Each domain's most recent `DomainSummary`
/// synthesis object is folded in.
fn gather_domain_outputs(
    rt: &FfiRuntime,
    tenant: &memory_manager::TenantMemoryObject,
) -> Vec<DomainOutput> {
    let mut outputs = Vec::with_capacity(tenant.domain_scopes.len());
    for domain_scope in &tenant.domain_scopes {
        if let Some(object) =
            newest_object_for_scope_of_type(rt, *domain_scope, SynthesisObjectType::DomainSummary)
        {
            match DomainOutput::from_domain_object(object) {
                Ok(o) => outputs.push(o),
                Err(e) => {
                    tracing::warn!(
                        domain = %domain_scope.as_uuid(),
                        error = ?e,
                        "skipping domain output that failed hierarchy validation",
                    );
                }
            }
        } else if let Some(dmo) = rt.domain_memory(*domain_scope) {
            if dmo.recap.is_empty() {
                continue;
            }
            // Synthesize a DomainSummary on the fly mirroring the channel
            // fallback in `gather_channel_outputs` so tenant-tier dispatch
            // is not blocked by a missing on-disk SynthesisObject row.
            let object = SynthesisObject::new(
                *domain_scope,
                WindowId::new_v4(),
                SynthesisObjectType::DomainSummary,
                dmo.recap.as_bytes().to_vec(),
                Uuid::nil(),
            );
            match DomainOutput::from_domain_object(object) {
                Ok(o) => outputs.push(o),
                Err(e) => {
                    tracing::warn!(
                        domain = %domain_scope.as_uuid(),
                        error = ?e,
                        "synthesised DomainSummary rejected by hierarchy validator",
                    );
                }
            }
        }
    }
    outputs
}

fn newest_channel_recap_for_scope(rt: &FfiRuntime, scope: ScopeId) -> Option<SynthesisObject> {
    newest_object_for_scope_of_type(rt, scope, SynthesisObjectType::ChannelRecap)
}

fn newest_object_for_scope_of_type(
    rt: &FfiRuntime,
    scope: ScopeId,
    kind: SynthesisObjectType,
) -> Option<SynthesisObject> {
    rt.synthesis_objects
        .values()
        .filter(|o| o.scope_id == scope && o.object_type == kind)
        .max_by_key(|o| o.created_at)
        .cloned()
}

/// Transition the live `SynthesisWindowManager` window into `Failed`.
///
/// Phase 2 mutates a cloned manager so on the live manager the window
/// is still in `Pending`. `mark_failed` only accepts the
/// `InProgress → Failed` transition, so we replay the
/// `Pending → InProgress → Failed` chain here. Both steps are
/// best-effort because `apply_dispatch_outcome` is already on a
/// failure path — surfacing a deeper error from the bookkeeping here
/// would mask the real synthesis failure that the caller is about
/// to receive. A `mark_in_progress` refusal logs at `warn`; the
/// subsequent `mark_failed` will still attempt the transition and
/// also log on refusal so an operator can correlate stuck windows.
fn fail_window_on_live_manager(rt: &mut FfiRuntime, window_id: WindowId, reason: &str) {
    if let Err(e) = rt.synthesis_windows.mark_in_progress(window_id) {
        tracing::warn!(
            window = %window_id.as_uuid(),
            error = ?e,
            reason,
            "fail_window_on_live_manager: mark_in_progress refused",
        );
    }
    if let Err(e) = rt.synthesis_windows.mark_failed(window_id) {
        tracing::warn!(
            window = %window_id.as_uuid(),
            error = ?e,
            reason,
            "fail_window_on_live_manager: mark_failed refused; window left in current status",
        );
    }
}

/// Newest `Complete` window for `(scope, tier)`, used by the
/// cooldown short-circuit so a recent Domain completion cannot
/// surface as the "result" of a Tenant request (or vice versa).
///
/// `SynthesisWindow` does not store the tier directly — the
/// pipeline keeps tier on the [`TieredWindowHandle`] returned by
/// `open_tiered_window` rather than on the persisted window
/// shape. We therefore look up the matching
/// [`synthesis_pipeline::SynthesisObject`] (one per completed
/// window) and filter by its `object_type` against the expected
/// tier-specific output type. `Pending` / `InProgress` / `Failed`
/// windows are skipped because they have no associated object yet
/// — and the cooldown contract only short-circuits when a
/// matching-tier *Complete* window is available.
fn newest_complete_window(
    rt: &FfiRuntime,
    scope: ScopeId,
    tier: SynthesisTierKind,
) -> Option<WindowId> {
    let expected_object_type = match tier {
        SynthesisTierKind::Domain => SynthesisObjectType::DomainSummary,
        SynthesisTierKind::Tenant => SynthesisObjectType::TenantSummary,
    };
    rt.synthesis_windows
        .windows_for(scope)
        .iter()
        .filter(|w| w.status == WindowStatus::Complete)
        .filter(|w| {
            rt.synthesis_objects
                .get(&w.id)
                .is_some_and(|o| o.object_type == expected_object_type)
        })
        .max_by_key(|w| w.window_end)
        .map(|w| w.id)
}

/// Lower-hex encode a BLAKE3 content hash for the FFI surface.
/// Produced once per row by [`admit_approved_document`] /
/// [`list_approved_documents`]; the 64-char output is wire-flat
/// and stable across host languages.
///
/// Inlined here rather than pulling the `hex` crate as a dep —
/// the FFI surface only formats fixed-size BLAKE3 hashes, and a
/// 0.5 KiB lookup table is faster than the generic `hex::encode`
/// for this single shape.
fn encode_content_hash_hex(hash: &crypto::ContentHash) -> String {
    const TABLE: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(crypto::CONTENT_HASH_LEN * 2);
    for byte in hash {
        out.push(TABLE[(byte >> 4) as usize] as char);
        out.push(TABLE[(byte & 0x0f) as usize] as char);
    }
    out
}

/// Validate a Phase-8 approved-document metadata field
/// (`label` or `approver`). Empty / overlong strings are rejected
/// with [`FfiError::Memory`] whose message names the field, the
/// observed length, and the cap so the host can fix the call site
/// without guessing which input was rejected.
fn validate_approved_document_metadata(field: &'static str, value: &str) -> FfiResult<()> {
    if value.is_empty() {
        return Err(FfiError::Memory {
            message: format!("admit_approved_document: {field} must be non-empty"),
        });
    }
    if value.len() > MAX_APPROVED_DOCUMENT_METADATA_CHARS {
        return Err(FfiError::Memory {
            message: format!(
                "admit_approved_document: {field} length {} bytes exceeds the {} byte cap",
                value.len(),
                MAX_APPROVED_DOCUMENT_METADATA_CHARS,
            ),
        });
    }
    Ok(())
}

fn parse_window_id(s: &str) -> FfiResult<WindowId> {
    let uuid = Uuid::parse_str(s).map_err(|e| FfiError::InvalidId {
        message: format!("invalid synthesis window id `{s}`: {e}"),
    })?;
    Ok(WindowId::from_uuid(uuid))
}

fn parse_scope_bindings(bindings: Option<&[String]>) -> FfiResult<Option<Vec<Uuid>>> {
    let Some(bindings) = bindings else {
        return Ok(None);
    };
    let mut out = Vec::with_capacity(bindings.len());
    for s in bindings {
        let uuid = Uuid::parse_str(s).map_err(|e| FfiError::InvalidId {
            message: format!("invalid scope binding `{s}`: {e}"),
        })?;
        out.push(uuid);
    }
    Ok(Some(out))
}

fn endpoint_config_from_ffi(cfg: &SynthesisEngineConfig) -> FfiResult<EndpointConfig> {
    // Reject obviously-bad timeouts before they reach reqwest. A
    // host that passes `u64::MAX` (or any value beyond
    // [`MAX_TIMEOUT_MS`]) would otherwise create a
    // `Duration::from_millis` of ~584 million years, which
    // `reqwest::blocking::Client::builder().timeout()` accepts
    // verbatim — effectively disabling the per-request timeout and
    // letting a wedged endpoint hold the dispatch thread
    // indefinitely. The cap is a defence-in-depth guard around the
    // 5-minute per-(scope, tier) cooldown contract; surfacing
    // `Unavailable` (rather than silently clamping) keeps host
    // misconfiguration loud instead of papering over it.
    if cfg.timeout_ms > MAX_TIMEOUT_MS {
        return Err(FfiError::Unavailable {
            subsystem: format!(
                "synthesis_engine (timeout_ms={} exceeds MAX_TIMEOUT_MS={})",
                cfg.timeout_ms, MAX_TIMEOUT_MS,
            ),
        });
    }
    let mut endpoint = EndpointConfig::new(
        cfg.url.clone(),
        cfg.api_key_ref.clone(),
        cfg.model_id.clone(),
    );
    if cfg.max_tokens > 0 {
        endpoint = endpoint.with_max_tokens(cfg.max_tokens);
    }
    if cfg.timeout_ms > 0 {
        endpoint = endpoint.with_timeout(Duration::from_millis(cfg.timeout_ms));
    }
    if let Some(grammar) = cfg.grammar.as_ref() {
        endpoint = endpoint.with_grammar(grammar.clone());
    }
    Ok(endpoint)
}

fn window_to_record(
    window: &synthesis_pipeline::SynthesisWindow,
    rt: &FfiRuntime,
) -> SynthesisStatusRecord {
    // Look up the synthesis object for this window so callers can
    // pull the artefact id without a second round trip.
    let object_id = rt
        .synthesis_objects
        .get(&window.id)
        .map(|o| o.id.as_uuid().to_string());
    // Tier resolution priority:
    //
    // 1. The tier stamped on the window at open time
    //    (`SynthesisWindow::tier`). This is the authoritative
    //    source and is populated for every window opened via
    //    [`HierarchyEnforcedWindowManager::open_tiered_window`]
    //    (the FFI dispatch path), so windows in `Pending`,
    //    `InProgress`, or `Failed` status report the correct tier
    //    without needing the `Complete` synthesis object.
    //
    // 2. The matching synthesis object's `object_type`. Used as a
    //    fallback for blobs persisted before the `tier` field was
    //    introduced (those rehydrate with `tier: None` per the
    //    `#[serde(default)]` annotation) and for windows opened via
    //    the legacy non-tiered `open_window` path.
    //
    // 3. `"unknown"` as a last-resort label so the host can still
    //    surface the window in `list_recent_syntheses` without
    //    crashing — this can only happen if a window was opened via
    //    `open_window` (no tier stamp) AND no synthesis object yet
    //    exists for it.
    let tier = match window.tier {
        Some(WindowScopeTier::Channel) => "channel".to_string(),
        Some(WindowScopeTier::Domain) => "domain".to_string(),
        Some(WindowScopeTier::Tenant) => "tenant".to_string(),
        None => rt
            .synthesis_objects
            .get(&window.id)
            .map_or("unknown", |o| match o.object_type {
                SynthesisObjectType::ChannelRecap => "channel",
                SynthesisObjectType::DomainSummary => "domain",
                SynthesisObjectType::TenantSummary => "tenant",
                SynthesisObjectType::EpisodicSummary => "episodic",
            })
            .to_string(),
    };
    SynthesisStatusRecord {
        synthesis_id: window.id.as_uuid().to_string(),
        scope_id: window.scope_id.as_uuid().to_string(),
        tier,
        status: window.status.as_str().to_string(),
        window_start_unix: window.window_start.timestamp(),
        window_end_unix: window.window_end.timestamp(),
        object_id,
    }
}

#[cfg(test)]
mod tests {
    //! FFI-level tests for the Phase 7 server-side synthesis surface.
    //!
    //! These tests open a real temp-dir-backed evidence store, install
    //! a deterministic [`ManagedEndpointSynthesizer`] (from the
    //! `synthesis_engine` crate's test-stub module — the production
    //! [`HttpManagedEndpointSynthesizer`] is exercised in
    //! `synthesis_engine`'s own test suite), and drive the public
    //! FFI surface against it. The HTTP transport itself is gated
    //! behind `--features http-client` and is not exercised here.

    use std::sync::Arc;

    use chrono::{Duration as ChronoDuration, Utc};
    use evidence_store::ScopeId;
    use memory_manager::{ChannelMemoryObject, DomainMemoryObject, TenantMemoryObject};
    use synthesis_engine::{ManagedEndpointSynthesizer, SynthesisEngine};
    use synthesis_pipeline::{
        HierarchyEnforcedWindowManager, SynthesisObject, SynthesisObjectType, WindowScopeTier,
    };
    use uuid::Uuid;

    use super::*;
    use crate::runtime::{close_store, open_store, with_runtime};

    /// Open a fresh evidence store backed by a temp dir. The
    /// returned `TempDir` MUST outlive the handle.
    fn fresh_store() -> (RuntimeHandle, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("evidence.db");
        let key_hex = "a5".repeat(32);
        let handle = open_store(path.to_string_lossy().into_owned(), key_hex)
            .expect("open_store should succeed");
        (handle, dir)
    }

    fn teardown(handle: RuntimeHandle) {
        let _ = close_store(handle);
    }

    /// Install the deterministic [`ManagedEndpointSynthesizer`] on
    /// `handle`. We poke `rt.synthesis_engine` directly because the
    /// public `configure_synthesis_engine` insists on building an
    /// HTTP-backed engine, which we explicitly do NOT want to
    /// exercise in unit tests.
    fn install_test_engine(handle: RuntimeHandle) {
        with_runtime(handle, |rt| {
            let engine: Arc<dyn SynthesisEngine> = Arc::new(ManagedEndpointSynthesizer::new());
            rt.synthesis_engine = Some(engine);
            rt.synthesis_scope_bindings = None;
            Ok(())
        })
        .expect("install_test_engine");
    }

    /// Seed a fully-populated domain memory + matching channel
    /// memories on `handle`. Returns the domain scope id.
    fn seed_domain_with_two_channels(handle: RuntimeHandle) -> ScopeId {
        let domain_scope = ScopeId::new_v4();
        let chan_a = ScopeId::new_v4();
        let chan_b = ScopeId::new_v4();

        with_runtime(handle, |rt| {
            let mut domain = DomainMemoryObject::new(domain_scope);
            domain.attach_channel_scope(chan_a);
            domain.attach_channel_scope(chan_b);
            rt.save_domain_memory(domain_scope, domain)?;

            let mut cma = ChannelMemoryObject::new(chan_a);
            cma.update_recap("alpha channel recap text", None);
            rt.save_channel_memory(chan_a, cma)?;
            let mut cmb = ChannelMemoryObject::new(chan_b);
            cmb.update_recap("beta channel recap text", None);
            rt.save_channel_memory(chan_b, cmb)?;
            Ok(())
        })
        .expect("seed domain");
        domain_scope
    }

    /// Seed a tenant memory plus one feeding domain (with its own
    /// channel) so tenant-tier synthesis has admissible inputs.
    fn seed_tenant_with_domain(handle: RuntimeHandle) -> ScopeId {
        let tenant_scope = ScopeId::new_v4();
        let domain_scope = ScopeId::new_v4();
        let channel_scope = ScopeId::new_v4();

        with_runtime(handle, |rt| {
            let mut tenant = TenantMemoryObject::new(tenant_scope);
            tenant.attach_domain_scope(domain_scope);
            rt.save_tenant_memory(tenant_scope, tenant)?;

            let mut domain = DomainMemoryObject::new(domain_scope);
            domain.attach_channel_scope(channel_scope);
            // A non-empty recap lets `gather_domain_outputs` build a
            // synthetic `DomainSummary` on the fly so the tenant
            // dispatch sees at least one feeder output.
            domain.update_recap("domain recap feeding tenant", None);
            rt.save_domain_memory(domain_scope, domain)?;

            let mut cmo = ChannelMemoryObject::new(channel_scope);
            cmo.update_recap("channel recap below domain", None);
            rt.save_channel_memory(channel_scope, cmo)?;
            Ok(())
        })
        .expect("seed tenant");
        tenant_scope
    }

    // ─────────────────────── configure_synthesis_engine ───────────

    #[test]
    fn install_test_engine_populates_slot() {
        let (handle, _dir) = fresh_store();
        install_test_engine(handle);

        let has_engine =
            with_runtime(handle, |rt| Ok(rt.synthesis_engine.is_some())).expect("with_runtime");
        assert!(has_engine, "engine slot must be populated");
        teardown(handle);
    }

    // ─────────────────────── trigger_server_synthesis ─────────────

    #[test]
    fn trigger_server_synthesis_without_engine_returns_unavailable() {
        let (handle, _dir) = fresh_store();
        let scope = seed_domain_with_two_channels(handle);
        let err = trigger_server_synthesis(
            handle,
            scope.as_uuid().to_string(),
            SynthesisTierKind::Domain,
        )
        .unwrap_err();
        assert!(
            matches!(err, FfiError::Unavailable { ref subsystem } if subsystem.contains("synthesis_engine")),
            "expected Unavailable(synthesis_engine), got {err:?}",
        );
        teardown(handle);
    }

    #[test]
    fn trigger_server_synthesis_on_forgotten_scope_returns_not_found() {
        let (handle, _dir) = fresh_store();
        install_test_engine(handle);
        let scope = seed_domain_with_two_channels(handle);
        // Forget the scope. Subsequent dispatches must short-circuit
        // before reaching the engine.
        crate::forget_scope(handle, scope.as_uuid().to_string()).expect("forget_scope");

        let err = trigger_server_synthesis(
            handle,
            scope.as_uuid().to_string(),
            SynthesisTierKind::Domain,
        )
        .unwrap_err();
        assert!(
            matches!(err, FfiError::NotFound { .. }),
            "expected NotFound, got {err:?}",
        );
        teardown(handle);
    }

    #[test]
    fn trigger_server_synthesis_domain_with_test_engine_produces_complete_window() {
        let (handle, _dir) = fresh_store();
        install_test_engine(handle);
        let scope = seed_domain_with_two_channels(handle);

        let window_id_str = trigger_server_synthesis(
            handle,
            scope.as_uuid().to_string(),
            SynthesisTierKind::Domain,
        )
        .expect("domain synthesis");

        // The returned id must parse as a UUID.
        let _: Uuid = window_id_str.parse().expect("returned id must be UUID");

        // synthesis_status reports Complete.
        let rec = synthesis_status(handle, window_id_str.clone()).expect("status");
        assert_eq!(rec.synthesis_id, window_id_str);
        assert_eq!(rec.scope_id, scope.as_uuid().to_string());
        assert_eq!(rec.tier, "domain");
        assert_eq!(rec.status, "complete");
        assert!(
            rec.object_id.is_some(),
            "complete window must carry object_id"
        );

        // list_recent_syntheses surfaces the same row.
        let list = list_recent_syntheses(handle, scope.as_uuid().to_string()).expect("list");
        assert!(
            list.iter().any(|r| r.synthesis_id == window_id_str),
            "list must include the newly-completed window",
        );
        teardown(handle);
    }

    #[test]
    fn trigger_server_synthesis_tenant_with_test_engine_produces_complete_window() {
        let (handle, _dir) = fresh_store();
        install_test_engine(handle);
        let scope = seed_tenant_with_domain(handle);

        let window_id_str = trigger_server_synthesis(
            handle,
            scope.as_uuid().to_string(),
            SynthesisTierKind::Tenant,
        )
        .expect("tenant synthesis");

        let rec = synthesis_status(handle, window_id_str.clone()).expect("status");
        assert_eq!(rec.tier, "tenant");
        assert_eq!(rec.status, "complete");

        // The tenant memory should now reference the latest synthesis
        // window via `last_synthesis_window`.
        let last = with_runtime(handle, |rt| {
            Ok(rt
                .tenant_memories
                .get(&scope)
                .and_then(|t| t.last_synthesis_window))
        })
        .expect("with_runtime");
        let want: Uuid = window_id_str.parse().unwrap();
        assert_eq!(last, Some(want));
        teardown(handle);
    }

    // ─────────────────────── synthesis_status ─────────────────────

    #[test]
    fn synthesis_status_returns_not_found_for_unknown_id() {
        let (handle, _dir) = fresh_store();
        let unknown = Uuid::new_v4().to_string();
        let err = synthesis_status(handle, unknown).unwrap_err();
        assert!(matches!(err, FfiError::NotFound { .. }));
        teardown(handle);
    }

    // ─────────────────────── list_recent_syntheses ────────────────

    #[test]
    fn list_recent_syntheses_returns_empty_for_unknown_scope() {
        let (handle, _dir) = fresh_store();
        // Unknown but well-formed scope id — list_recent_syntheses
        // must NOT error, it must return an empty list (the host's
        // status pane polls this on every visit).
        let scope = ScopeId::new_v4();
        let rows = list_recent_syntheses(handle, scope.as_uuid().to_string()).expect("list");
        assert!(rows.is_empty());
        teardown(handle);
    }

    #[test]
    fn list_recent_syntheses_filters_by_scope() {
        let (handle, _dir) = fresh_store();
        install_test_engine(handle);

        let scope_a = seed_domain_with_two_channels(handle);
        let scope_b = seed_domain_with_two_channels(handle);

        let win_a = trigger_server_synthesis(
            handle,
            scope_a.as_uuid().to_string(),
            SynthesisTierKind::Domain,
        )
        .expect("synthesise A");
        let win_b = trigger_server_synthesis(
            handle,
            scope_b.as_uuid().to_string(),
            SynthesisTierKind::Domain,
        )
        .expect("synthesise B");
        assert_ne!(win_a, win_b);

        let rows_a = list_recent_syntheses(handle, scope_a.as_uuid().to_string()).expect("list A");
        assert!(rows_a
            .iter()
            .all(|r| r.scope_id == scope_a.as_uuid().to_string()));
        assert!(rows_a.iter().any(|r| r.synthesis_id == win_a));
        assert!(rows_a.iter().all(|r| r.synthesis_id != win_b));
        teardown(handle);
    }

    // ─────────────────────── cooldown / retention / forgetting ────

    #[test]
    fn cooldown_short_circuits_redundant_dispatch() {
        let (handle, _dir) = fresh_store();
        install_test_engine(handle);
        let scope = seed_domain_with_two_channels(handle);

        let win1 = trigger_server_synthesis(
            handle,
            scope.as_uuid().to_string(),
            SynthesisTierKind::Domain,
        )
        .expect("first synthesis");
        // Second dispatch within the per-scope cooldown returns the
        // SAME window id (no new run was dispatched).
        let win2 = trigger_server_synthesis(
            handle,
            scope.as_uuid().to_string(),
            SynthesisTierKind::Domain,
        )
        .expect("cooldown short-circuit");
        assert_eq!(win1, win2, "cooldown must reuse the prior window id");

        // Force the cooldown stamp into the past to allow a new run.
        with_runtime(handle, |rt| {
            let past = Utc::now() - ChronoDuration::seconds(PER_SCOPE_COOLDOWN_SECS + 60);
            rt.synthesis_cooldowns
                .insert((scope, SynthesisTierKind::Domain), past);
            Ok(())
        })
        .expect("with_runtime");
        let win3 = trigger_server_synthesis(
            handle,
            scope.as_uuid().to_string(),
            SynthesisTierKind::Domain,
        )
        .expect("post-cooldown synthesis");
        assert_ne!(win3, win1, "post-cooldown dispatch must mint a new window");
        teardown(handle);
    }

    /// Regression test for the cross-tier cooldown short-circuit
    /// bug: a Domain completion on `scope` must NOT throttle a
    /// subsequent Tenant request on the same `scope`. The
    /// cooldown map is keyed by `(scope, tier)` so each tier runs
    /// its own clock. The test also asserts that the Tenant
    /// dispatch returns a freshly-minted window id (not the
    /// recycled Domain window id), which is the user-visible
    /// symptom the original bug surfaced.
    #[test]
    fn cooldown_does_not_leak_across_tiers() {
        let (handle, _dir) = fresh_store();
        install_test_engine(handle);

        // Single scope that doubles as Domain + Tenant so the
        // cooldown map sees `(scope, Domain)` AND `(scope, Tenant)`
        // back-to-back. In production the host hierarchies map a
        // scope to a single tier; the FFI still has to behave
        // correctly when a host registers both, because nothing
        // in the substrate forbids it and the bug would otherwise
        // silently return a wrong-tier window.
        let scope = ScopeId::new_v4();
        let chan = ScopeId::new_v4();
        let feeding_domain = ScopeId::new_v4();
        let feeding_channel = ScopeId::new_v4();
        with_runtime(handle, |rt| {
            // Domain side: one channel attached so domain
            // synthesis has admissible inputs.
            let mut domain = DomainMemoryObject::new(scope);
            domain.attach_channel_scope(chan);
            rt.save_domain_memory(scope, domain)?;
            let mut cmo = ChannelMemoryObject::new(chan);
            cmo.update_recap("channel recap for domain tier", None);
            rt.save_channel_memory(chan, cmo)?;

            // Tenant side: one feeding domain (different scope so
            // we don't collide with the domain memory above) and
            // its own channel.
            let mut tenant = TenantMemoryObject::new(scope);
            tenant.attach_domain_scope(feeding_domain);
            rt.save_tenant_memory(scope, tenant)?;
            let mut feeder = DomainMemoryObject::new(feeding_domain);
            feeder.attach_channel_scope(feeding_channel);
            feeder.update_recap("feeder domain recap", None);
            rt.save_domain_memory(feeding_domain, feeder)?;
            let mut feeder_chan = ChannelMemoryObject::new(feeding_channel);
            feeder_chan.update_recap("feeder channel recap", None);
            rt.save_channel_memory(feeding_channel, feeder_chan)?;
            Ok(())
        })
        .expect("seed cross-tier scope");

        let domain_win = trigger_server_synthesis(
            handle,
            scope.as_uuid().to_string(),
            SynthesisTierKind::Domain,
        )
        .expect("domain dispatch");

        // Within the cooldown window: a Tenant request on the
        // same scope must NOT be short-circuited by the Domain
        // stamp. The returned window id must be a fresh Tenant
        // window, not the recycled Domain window.
        let tenant_win = trigger_server_synthesis(
            handle,
            scope.as_uuid().to_string(),
            SynthesisTierKind::Tenant,
        )
        .expect("tenant dispatch");
        assert_ne!(
            domain_win, tenant_win,
            "Tenant request must NOT recycle the Domain window via cross-tier cooldown",
        );

        // Belt and braces: confirm the stored object types match
        // each request so a future regression that returns the
        // wrong-tier window still fails this test loudly.
        with_runtime(handle, |rt| {
            let domain_win_id =
                synthesis_pipeline::WindowId::from_uuid(domain_win.parse::<Uuid>().expect("uuid"));
            let tenant_win_id =
                synthesis_pipeline::WindowId::from_uuid(tenant_win.parse::<Uuid>().expect("uuid"));
            let domain_obj = rt
                .synthesis_objects
                .get(&domain_win_id)
                .expect("domain object");
            let tenant_obj = rt
                .synthesis_objects
                .get(&tenant_win_id)
                .expect("tenant object");
            assert_eq!(domain_obj.object_type, SynthesisObjectType::DomainSummary);
            assert_eq!(tenant_obj.object_type, SynthesisObjectType::TenantSummary);

            // Both cooldown stamps must be present and
            // independent (Domain throttles only Domain,
            // Tenant throttles only Tenant).
            assert!(rt
                .synthesis_cooldowns
                .contains_key(&(scope, SynthesisTierKind::Domain)));
            assert!(rt
                .synthesis_cooldowns
                .contains_key(&(scope, SynthesisTierKind::Tenant)));
            Ok(())
        })
        .expect("verify cross-tier state");

        // A *second* Domain request on the same scope DOES hit the
        // Domain cooldown (recycles the original Domain window),
        // proving the cooldown still works within a single tier.
        let domain_win_again = trigger_server_synthesis(
            handle,
            scope.as_uuid().to_string(),
            SynthesisTierKind::Domain,
        )
        .expect("second domain dispatch");
        assert_eq!(
            domain_win, domain_win_again,
            "Domain cooldown must still recycle within the same tier",
        );

        teardown(handle);
    }

    /// Regression test for the persist-after-prune bug: pruning a
    /// completed window must also rewrite the per-scope
    /// synthesis-object blob on disk so a subsequent
    /// `open_store` does NOT rehydrate the pruned objects as
    /// orphans. We drive the cycle by:
    ///
    /// 1. Filling the per-scope window list past
    ///    `WINDOW_RETENTION_CAP_PER_SCOPE` so pruning will fire,
    /// 2. Triggering one final synthesis (which prunes the
    ///    oldest objects in-memory + flushes both windows and
    ///    objects), and
    /// 3. Closing + reopening the store to drive rehydration.
    ///
    /// Post-rehydrate the in-memory `synthesis_objects` map must
    /// contain ONLY the post-prune subset — no orphans whose
    /// `window_id` no longer maps to a tracked window.
    #[test]
    fn prune_persists_synthesis_objects_to_disk() {
        let (handle, dir) = fresh_store();
        install_test_engine(handle);
        let scope = seed_domain_with_two_channels(handle);

        // Stuff the per-scope window list past the retention cap
        // with completed windows + matching synthesis objects.
        // The values are not exercised by the engine — only
        // their persistence is.
        let extra: usize = WINDOW_RETENTION_CAP_PER_SCOPE + 5;
        let pre_existing: Vec<synthesis_pipeline::WindowId> = with_runtime(handle, |rt| {
            let mut ids = Vec::with_capacity(extra);
            let now = Utc::now();
            for i in 0..extra {
                let offset = i64::try_from(i + 1).expect("loop index fits i64");
                let end = now - ChronoDuration::hours(offset);
                let start = end - ChronoDuration::seconds(30);
                let h = rt
                    .synthesis_windows
                    .open_tiered_window(scope, WindowScopeTier::Domain, start, end)
                    .expect("open window");
                rt.synthesis_windows.mark_in_progress(h.window_id).unwrap();
                rt.synthesis_windows.mark_complete(h.window_id).unwrap();
                let obj = SynthesisObject::new(
                    scope,
                    h.window_id,
                    SynthesisObjectType::DomainSummary,
                    format!("preexisting recap #{i}").into_bytes(),
                    Uuid::nil(),
                );
                rt.save_synthesis_object(scope, obj)?;
                ids.push(h.window_id);
            }
            rt.flush_synthesis_windows()?;
            Ok(ids)
        })
        .expect("seed pre-existing windows");

        // Trigger one fresh synthesis. The post-apply prune fires
        // and must also rewrite the on-disk synthesis-object blob.
        let fresh = trigger_server_synthesis(
            handle,
            scope.as_uuid().to_string(),
            SynthesisTierKind::Domain,
        )
        .expect("fresh synthesis triggers prune");
        let fresh_id =
            synthesis_pipeline::WindowId::from_uuid(fresh.parse::<Uuid>().expect("uuid"));

        // After prune, the in-memory map should be at the cap +
        // the fresh window. The oldest pre-existing object should
        // be gone.
        let path = dir.path().join("evidence.db");
        let (remaining_in_memory, oldest_pre_existing) = with_runtime(handle, |rt| {
            let remaining: Vec<synthesis_pipeline::WindowId> = rt
                .synthesis_objects
                .keys()
                .filter(|id| pre_existing.contains(id) || **id == fresh_id)
                .copied()
                .collect();
            let oldest = *pre_existing.last().expect("non-empty");
            Ok((remaining, oldest))
        })
        .expect("with_runtime");
        assert!(
            !remaining_in_memory.contains(&oldest_pre_existing),
            "prune must drop the oldest pre-existing object from memory",
        );
        assert!(
            remaining_in_memory.contains(&fresh_id),
            "fresh window must be present after prune",
        );

        // Close + reopen the store. The on-disk synthesis-object
        // blob MUST reflect the post-prune state; without the
        // flush, rehydration would resurrect every object in
        // `pre_existing` (the original bug).
        teardown(handle);
        let key_hex = "a5".repeat(32);
        let handle2 = crate::runtime::open_store(path.to_string_lossy().into_owned(), key_hex)
            .expect("reopen");
        let resurrected: Vec<synthesis_pipeline::WindowId> = with_runtime(handle2, |rt| {
            Ok(rt
                .synthesis_objects
                .keys()
                .filter(|id| pre_existing.contains(id))
                .copied()
                .collect())
        })
        .expect("inspect rehydrated map");
        assert!(
            !resurrected.contains(&oldest_pre_existing),
            "pruned object must NOT resurrect from disk on open_store \
             (BUG_0002 regression)",
        );
        // Belt and braces: no rehydrated object should have a
        // window_id that is unknown to the rehydrated window
        // manager.
        with_runtime(handle2, |rt| {
            for id in rt.synthesis_objects.keys() {
                assert!(
                    rt.synthesis_windows.get(*id).is_some(),
                    "rehydrated synthesis object {id:?} has no matching window \
                     — disk blob is out of sync with the window manager",
                );
            }
            Ok(())
        })
        .expect("verify no orphans");
        teardown(handle2);
    }

    #[test]
    fn forget_scope_purges_synthesis_state() {
        let (handle, _dir) = fresh_store();
        install_test_engine(handle);
        let scope = seed_domain_with_two_channels(handle);

        let win = trigger_server_synthesis(
            handle,
            scope.as_uuid().to_string(),
            SynthesisTierKind::Domain,
        )
        .expect("synth");
        let win_uuid: Uuid = win.parse().unwrap();
        let win_id = synthesis_pipeline::WindowId::from_uuid(win_uuid);

        // Sanity: state present before forgetting.
        with_runtime(handle, |rt| {
            assert!(rt.domain_memories.contains_key(&scope));
            assert!(rt
                .synthesis_cooldowns
                .contains_key(&(scope, SynthesisTierKind::Domain)));
            assert!(rt.synthesis_objects.contains_key(&win_id));
            assert!(rt.synthesis_windows.get(win_id).is_some());
            Ok(())
        })
        .unwrap();

        crate::forget_scope(handle, scope.as_uuid().to_string()).expect("forget");

        // Every synthesis-adjacent map must drop the scope's entries.
        with_runtime(handle, |rt| {
            assert!(!rt.domain_memories.contains_key(&scope));
            assert!(!rt.tenant_memories.contains_key(&scope));
            // `retain` strips every tier whose first component
            // matches the forgotten scope, so neither Domain nor
            // Tenant cooldown stamps survive.
            assert!(!rt.synthesis_cooldowns.keys().any(|(s, _)| *s == scope));
            assert!(!rt.synthesis_objects.contains_key(&win_id));
            assert!(rt.synthesis_windows.get(win_id).is_none());
            assert!(rt.synthesis_windows.windows_for(scope).is_empty());
            Ok(())
        })
        .unwrap();
        teardown(handle);
    }

    /// Regression test for BUG_0001 + ANALYSIS_0004 (Devin Review on
    /// commit 6456b6f): the `SynthesisWindowManager` is persisted
    /// under a single sentinel scope, so its on-disk blob contains
    /// windows for every scope mixed together. When
    /// `forget_scope_state`'s post-prune `flush_synthesis_windows`
    /// fails (disk-full, crash before flush, etc.) the tombstone
    /// landed in `forgotten_scopes` but the windows for the
    /// forgotten scope remained in the sentinel blob — and the old
    /// `open_store` did not filter them out, so the orphan windows
    /// resurrected across restarts.
    ///
    /// This test simulates the partial-forget condition by writing
    /// the tombstone row directly via `record_forgotten_scope`,
    /// bypassing the in-memory cleanup that `forget_scope_state`
    /// would otherwise perform. After close + reopen the
    /// tombstone-aware rehydration cleanup must drop the orphan
    /// window from the rehydrated manager.
    #[test]
    fn open_store_purges_tombstoned_scope_windows_from_rehydrated_manager() {
        let (handle, dir) = fresh_store();
        install_test_engine(handle);
        let scope = seed_domain_with_two_channels(handle);

        // Phase 1: trigger a synthesis so a window lands on disk
        // under the sentinel-scope blob.
        let win = trigger_server_synthesis(
            handle,
            scope.as_uuid().to_string(),
            SynthesisTierKind::Domain,
        )
        .expect("synth");
        let win_id =
            synthesis_pipeline::WindowId::from_uuid(win.parse::<Uuid>().expect("uuid parse"));

        // Phase 2: write the scope tombstone directly to the
        // `forgotten_scopes` table, bypassing `forget_scope_state`.
        // This is the exact corruption shape produced when
        // `forget_scope_state`'s post-cleanup
        // `flush_synthesis_windows` errors out: the tombstone row
        // landed but the synthesis-windows blob still references
        // the window.
        with_runtime(handle, |rt| {
            rt.store_mut()
                .record_forgotten_scope(scope)
                .expect("record tombstone");
            Ok(())
        })
        .expect("with_runtime");

        // Phase 3: close + reopen the store. The rehydration
        // cleanup must drop the orphan window.
        let path = dir.path().join("evidence.db");
        teardown(handle);
        let key_hex = "a5".repeat(32);
        let handle2 = crate::runtime::open_store(path.to_string_lossy().into_owned(), key_hex)
            .expect("reopen");

        with_runtime(handle2, |rt| {
            assert!(
                rt.synthesis_windows.get(win_id).is_none(),
                "BUG_0001 regression: window for tombstoned scope must NOT resurrect on \
                 open_store via the rehydrated SynthesisWindowManager",
            );
            assert!(
                rt.synthesis_windows.windows_for(scope).is_empty(),
                "ANALYSIS_0004 regression: no orphan windows for the tombstoned scope may \
                 survive the open_store rehydration cleanup",
            );
            // Belt-and-braces: the synthesis_objects map must
            // also be free of orphans pointing at the dropped
            // window. The pre-existing rehydration path already
            // skips tombstoned scopes when loading per-scope
            // synthesis_object rows, so this is a sanity check
            // on the two paths staying in sync.
            assert!(
                !rt.synthesis_objects.contains_key(&win_id),
                "synthesis_objects must not retain orphan entry for window of tombstoned scope",
            );
            Ok(())
        })
        .expect("inspect post-reopen state");

        // Reopen a second time. The cleanup should have rewritten
        // the on-disk blob so the second open observes the same
        // post-cleanup state — i.e. the fix is durable across
        // restarts, not just a one-shot in-memory filter.
        teardown(handle2);
        let key_hex2 = "a5".repeat(32);
        let handle3 = crate::runtime::open_store(path.to_string_lossy().into_owned(), key_hex2)
            .expect("second reopen");
        with_runtime(handle3, |rt| {
            assert!(
                rt.synthesis_windows.windows_for(scope).is_empty(),
                "second open_store must observe the persisted cleanup; the sentinel blob \
                 should have been rewritten on the first reopen",
            );
            Ok(())
        })
        .expect("inspect second-reopen state");
        teardown(handle3);
    }

    /// ANALYSIS_0006 regression: orphan synthesis objects (whose
    /// `window_id` no longer corresponds to any tracked window) must
    /// be purged at `open_store` time AND the divergent on-disk blob
    /// must be rewritten so subsequent opens don't pay the cleanup
    /// cost twice.
    ///
    /// We simulate the divergent-flush condition by triggering a
    /// real synthesis (both blobs land on disk), then mutating only
    /// the in-memory windows manager + flushing windows-only. The
    /// per-scope synthesis-object blob keeps referencing the now-
    /// removed window. On the next `open_store` the orphan must be
    /// dropped in-memory AND the on-disk synthesis-object blob must
    /// be rewritten.
    #[test]
    fn open_store_purges_orphan_synthesis_objects_and_rewrites_blob() {
        let (handle, dir) = fresh_store();
        install_test_engine(handle);
        let scope = seed_domain_with_two_channels(handle);

        // Phase 1: real synthesis dispatch — windows + objects
        // both land on disk under the per-scope synthesis-object
        // row.
        let win = trigger_server_synthesis(
            handle,
            scope.as_uuid().to_string(),
            SynthesisTierKind::Domain,
        )
        .expect("synth");
        let win_id =
            synthesis_pipeline::WindowId::from_uuid(win.parse::<Uuid>().expect("uuid parse"));

        // Phase 2: simulate the divergent-flush failure mode. The
        // happy path flushes both blobs after a successful synth;
        // we mutate only the in-memory windows manager and flush
        // windows-only so the on-disk state looks exactly like what
        // a host would observe after `flush_synthesis_windows`
        // succeeded and `flush_synthesis_objects` failed.
        with_runtime(handle, |rt| {
            rt.synthesis_windows.remove_windows_for_scope(scope);
            rt.flush_synthesis_windows()
        })
        .expect("flush windows-only");

        // Phase 3: close + reopen. Orphan-aware cleanup must drop
        // the synthesis object from the rehydrated map.
        let path = dir.path().join("evidence.db");
        teardown(handle);
        let key_hex = "a5".repeat(32);
        let handle2 = crate::runtime::open_store(path.to_string_lossy().into_owned(), key_hex)
            .expect("reopen");

        with_runtime(handle2, |rt| {
            assert!(
                !rt.synthesis_objects.contains_key(&win_id),
                "ANALYSIS_0006 regression: orphan synthesis_object whose window_id is not in \
                 the rehydrated SynthesisWindowManager must be purged at open_store time",
            );
            assert!(
                rt.synthesis_windows.get(win_id).is_none(),
                "windows manager state must remain consistent across the orphan cleanup",
            );
            Ok(())
        })
        .expect("inspect post-reopen state");

        // Phase 4: reopen again — the cleanup must have rewritten
        // the on-disk synthesis-object blob, so the second reopen
        // observes the same post-cleanup state. If the rewrite
        // failed silently this assertion holds anyway (the orphan
        // cleanup is idempotent), but the tracing log would have
        // surfaced the rewrite failure on the first reopen.
        teardown(handle2);
        let handle3 =
            crate::runtime::open_store(path.to_string_lossy().into_owned(), "a5".repeat(32))
                .expect("second reopen");
        with_runtime(handle3, |rt| {
            assert!(
                !rt.synthesis_objects.contains_key(&win_id),
                "second open_store must observe the persisted orphan-cleanup; the per-scope \
                 synthesis_object blob should have been rewritten on the first reopen",
            );
            Ok(())
        })
        .expect("inspect second-reopen state");
        teardown(handle3);
    }

    /// ANALYSIS_0007 regression: synthesis status records for
    /// windows that have NOT reached `Complete` status must still
    /// report the correct tier, derived from the persisted
    /// `SynthesisWindow::tier` field. Before the fix, only
    /// `Complete` windows had a tier (inferred from the associated
    /// synthesis object's `object_type`); other statuses surfaced
    /// as `"unknown"`.
    #[test]
    fn synthesis_status_reports_tier_for_non_complete_windows() {
        let (handle, _dir) = fresh_store();
        let scope = seed_domain_with_two_channels(handle);

        // Open a `Pending` domain window directly via the manager
        // (skip the engine dispatch so the window never transitions
        // out of `Pending`).
        let now = chrono::Utc::now();
        let window_id = with_runtime(handle, |rt| {
            let h = rt
                .synthesis_windows
                .open_tiered_window(
                    scope,
                    synthesis_pipeline::WindowScopeTier::Domain,
                    now - chrono::Duration::hours(1),
                    now,
                )
                .expect("open_tiered_window");
            rt.flush_synthesis_windows().expect("flush windows");
            Ok(h.window_id)
        })
        .expect("with_runtime");

        let record =
            synthesis_status(handle, window_id.as_uuid().to_string()).expect("synthesis_status");
        assert_eq!(record.status, "pending");
        assert_eq!(
            record.tier, "domain",
            "ANALYSIS_0007 regression: Pending windows must surface the persisted tier",
        );
        assert!(record.object_id.is_none());

        // Tenant variant — same code path, different tier.
        let tenant_scope = ScopeId::new_v4();
        let tenant_window_id = with_runtime(handle, |rt| {
            rt.tenant_memory_mut(tenant_scope);
            let h = rt
                .synthesis_windows
                .open_tiered_window(
                    tenant_scope,
                    synthesis_pipeline::WindowScopeTier::Tenant,
                    now - chrono::Duration::hours(1),
                    now,
                )
                .expect("open tenant window");
            rt.synthesis_windows
                .mark_in_progress(h.window_id)
                .expect("mark in_progress");
            rt.flush_synthesis_windows().expect("flush windows");
            Ok(h.window_id)
        })
        .expect("with_runtime");

        let tenant_record = synthesis_status(handle, tenant_window_id.as_uuid().to_string())
            .expect("tenant synthesis_status");
        assert_eq!(tenant_record.status, "in_progress");
        assert_eq!(
            tenant_record.tier, "tenant",
            "ANALYSIS_0007 regression: InProgress windows must surface the persisted tier",
        );
        teardown(handle);
    }

    #[test]
    fn prune_completed_windows_caps_at_retention_limit() {
        let (handle, _dir) = fresh_store();
        let scope = ScopeId::new_v4();
        let total: usize = WINDOW_RETENTION_CAP_PER_SCOPE + 10;

        with_runtime(handle, |rt| {
            let now = Utc::now();
            for i in 0..total {
                // Stagger window_end so prune ordering is deterministic.
                let offset = i64::try_from(i).expect("loop index fits i64");
                let end = now - ChronoDuration::seconds(60 * offset);
                let start = end - ChronoDuration::seconds(30);
                let h = rt
                    .synthesis_windows
                    .open_tiered_window(scope, WindowScopeTier::Domain, start, end)
                    .expect("open window");
                rt.synthesis_windows.mark_in_progress(h.window_id).unwrap();
                rt.synthesis_windows.mark_complete(h.window_id).unwrap();
            }
            assert_eq!(rt.synthesis_windows.windows_for(scope).len(), total);

            let pruned = rt.prune_completed_windows(scope, WINDOW_RETENTION_CAP_PER_SCOPE);
            assert_eq!(pruned.len(), total - WINDOW_RETENTION_CAP_PER_SCOPE);
            assert_eq!(
                rt.synthesis_windows.windows_for(scope).len(),
                WINDOW_RETENTION_CAP_PER_SCOPE,
                "retention cap enforced",
            );
            Ok(())
        })
        .unwrap();
        teardown(handle);
    }

    /// Always-failing test engine that returns `EngineError::Engine`
    /// from both tier dispatchers. Used to drive the Phase 3 failure
    /// path so we can verify the live `SynthesisWindowManager` ends
    /// up in `Failed` (not stuck in `Pending`).
    struct FailingTestEngine;
    impl SynthesisEngine for FailingTestEngine {
        fn synthesize_domain(
            &self,
            _windows: &mut SynthesisWindowManager,
            _handle: TieredWindowHandle,
            _input: synthesis_pipeline::DomainSynthesisInput,
        ) -> synthesis_engine::Result<synthesis_engine::DomainSynthesisResult> {
            Err(synthesis_engine::EngineError::engine("test injection"))
        }
        fn synthesize_tenant(
            &self,
            _windows: &mut SynthesisWindowManager,
            _handle: TieredWindowHandle,
            _input: synthesis_pipeline::TenantSynthesisInput,
        ) -> synthesis_engine::Result<synthesis_engine::TenantSynthesisResult> {
            Err(synthesis_engine::EngineError::engine("test injection"))
        }
    }

    #[test]
    fn failing_engine_transitions_window_to_failed_not_pending() {
        // Regression test for the `mark_failed`-on-`Pending` bug.
        // Phase 2 mutates a cloned manager, so on the live manager
        // the window is still `Pending` when Phase 3 runs. The fix
        // replays `Pending → InProgress → Failed` so the live window
        // ends up `Failed`, surfacing the failure to operators and
        // letting future retention sweeps reason about the window.
        let (handle, _dir) = fresh_store();
        with_runtime(handle, |rt| {
            let engine: Arc<dyn SynthesisEngine> = Arc::new(FailingTestEngine);
            rt.synthesis_engine = Some(engine);
            rt.synthesis_scope_bindings = None;
            Ok(())
        })
        .unwrap();
        let scope = seed_domain_with_two_channels(handle);

        let err = trigger_server_synthesis(
            handle,
            scope.as_uuid().to_string(),
            SynthesisTierKind::Domain,
        )
        .expect_err("failing engine should bubble Synthesis error");
        assert!(matches!(err, FfiError::Synthesis { .. }));

        // The Phase-1 window must have transitioned to Failed (not
        // be stuck in Pending). It's also the only window on the
        // scope, so `list_recent_syntheses` returns exactly one row.
        let rows = list_recent_syntheses(handle, scope.as_uuid().to_string()).expect("list");
        assert_eq!(rows.len(), 1, "exactly one window opened for this scope");
        assert_eq!(
            rows[0].status, "failed",
            "live window must end Failed after dispatch error, not stuck Pending",
        );
        teardown(handle);
    }

    #[test]
    fn list_recent_syntheses_respects_cap() {
        let (handle, _dir) = fresh_store();
        let scope = ScopeId::new_v4();
        // Cram in more than the cap so the FFI surface has to
        // truncate.
        let total = LIST_RECENT_SYNTHESES_CAP + 5;

        with_runtime(handle, |rt| {
            let now = Utc::now();
            for i in 0..total {
                let offset = i64::try_from(i).expect("loop index fits i64");
                let end = now - ChronoDuration::seconds(60 * offset);
                let start = end - ChronoDuration::seconds(30);
                let h = rt
                    .synthesis_windows
                    .open_tiered_window(scope, WindowScopeTier::Domain, start, end)
                    .unwrap();
                rt.synthesis_windows.mark_in_progress(h.window_id).unwrap();
                rt.synthesis_windows.mark_complete(h.window_id).unwrap();
                // Attach a synthesis object so the tier reports
                // correctly (otherwise list_recent_syntheses reports
                // "unknown").
                let obj = SynthesisObject::new(
                    scope,
                    h.window_id,
                    SynthesisObjectType::DomainSummary,
                    b"recap".to_vec(),
                    Uuid::nil(),
                );
                rt.synthesis_objects.insert(h.window_id, obj);
            }
            Ok(())
        })
        .unwrap();

        let rows = list_recent_syntheses(handle, scope.as_uuid().to_string()).expect("list");
        assert_eq!(rows.len(), LIST_RECENT_SYNTHESES_CAP);
        // Sorted by window_end DESC: the first row should have the
        // largest window_end_unix.
        for w in rows.windows(2) {
            assert!(w[0].window_end_unix >= w[1].window_end_unix);
        }
        teardown(handle);
    }

    /// Pathological `timeout_ms` values must be rejected by the FFI
    /// before they reach `reqwest`. The hand-crafted config below
    /// exceeds `MAX_TIMEOUT_MS` by one millisecond — the FFI layer
    /// surfaces `Unavailable` with a message that names the offending
    /// value so host operators can immediately see why their config
    /// was refused.
    #[test]
    fn endpoint_config_from_ffi_rejects_timeout_above_cap() {
        let cfg = crate::types::SynthesisEngineConfig {
            url: "https://example.test/v1/synth".into(),
            api_key_ref: "SYNTH_KEY_REF".into(),
            model_id: "slm-recap-v1".into(),
            max_tokens: 512,
            timeout_ms: MAX_TIMEOUT_MS + 1,
            grammar: None,
            scope_bindings: None,
        };
        let err = endpoint_config_from_ffi(&cfg).expect_err("oversize timeout must reject");
        match err {
            FfiError::Unavailable { subsystem } => {
                assert!(
                    subsystem.contains("timeout_ms="),
                    "error message must surface the offending value, got: {subsystem}",
                );
                assert!(
                    subsystem.contains(&MAX_TIMEOUT_MS.to_string()),
                    "error message must surface the cap, got: {subsystem}",
                );
            }
            other => panic!("expected FfiError::Unavailable, got {other:?}"),
        }
    }

    /// Boundary case: `timeout_ms == MAX_TIMEOUT_MS` is accepted and
    /// forwarded to the underlying `EndpointConfig` verbatim.
    #[test]
    fn endpoint_config_from_ffi_accepts_timeout_at_cap() {
        let cfg = crate::types::SynthesisEngineConfig {
            url: "https://example.test/v1/synth".into(),
            api_key_ref: "SYNTH_KEY_REF".into(),
            model_id: "slm-recap-v1".into(),
            max_tokens: 512,
            timeout_ms: MAX_TIMEOUT_MS,
            grammar: None,
            scope_bindings: None,
        };
        let endpoint = endpoint_config_from_ffi(&cfg).expect("at-cap timeout must accept");
        assert_eq!(
            endpoint.timeout,
            Some(Duration::from_millis(MAX_TIMEOUT_MS))
        );
    }

    /// `timeout_ms == 0` keeps the documented "fall back to
    /// `DEFAULT_TIMEOUT`" semantic: the FFI does not forward a custom
    /// timeout to `EndpointConfig`, so the downstream default applies.
    #[test]
    fn endpoint_config_from_ffi_zero_timeout_uses_default() {
        let cfg = crate::types::SynthesisEngineConfig {
            url: "https://example.test/v1/synth".into(),
            api_key_ref: "SYNTH_KEY_REF".into(),
            model_id: "slm-recap-v1".into(),
            max_tokens: 512,
            timeout_ms: 0,
            grammar: None,
            scope_bindings: None,
        };
        let endpoint = endpoint_config_from_ffi(&cfg).expect("zero timeout must accept");
        // `EndpointConfig::timeout` is `None` until a custom value
        // is installed via `with_timeout`; the synthesis_engine layer
        // applies `DEFAULT_TIMEOUT` when the field is `None`.
        assert!(endpoint.timeout.is_none());
    }

    // ───────────────── Phase 8: approved-document payloads ────────

    /// Happy path: admitting an approved document persists the
    /// AEAD payload row, returns a populated
    /// [`ApprovedDocumentSummary`], and admits a matching ref onto
    /// the tenant memory. A subsequent `list_approved_documents`
    /// surfaces the ref joined with its payload metadata.
    #[test]
    fn admit_approved_document_persists_ref_and_payload() {
        let (handle, _dir) = fresh_store();
        let scope = seed_tenant_with_domain(handle);
        let scope_str = scope.as_uuid().to_string();

        let payload = b"OFFICIAL POLICY v3.2 - confidential".to_vec();
        let summary = admit_approved_document(
            handle,
            scope_str.clone(),
            "Tenant Policy v3.2".into(),
            "compliance-officer".into(),
            payload.clone(),
        )
        .expect("admit_approved_document");

        // Summary mirrors the on-disk row.
        assert_eq!(summary.scope_id, scope_str);
        assert_eq!(summary.label, "Tenant Policy v3.2");
        assert_eq!(summary.approver, "compliance-officer");
        assert_eq!(summary.payload_bytes, payload.len() as u64);
        assert_eq!(summary.content_hash_hex.len(), 64);
        assert!(summary
            .content_hash_hex
            .chars()
            .all(|c| c.is_ascii_hexdigit()));
        let doc_id_uuid = Uuid::parse_str(&summary.id).expect("doc id is a UUID");

        // Tenant memory holds the ref.
        let admitted_id = with_runtime(handle, |rt| {
            let tmo = rt.tenant_memory(scope).expect("tenant memory present");
            assert_eq!(tmo.approved_documents.len(), 1);
            Ok(tmo.approved_documents[0].id)
        })
        .expect("with_runtime");
        assert_eq!(admitted_id, doc_id_uuid);

        // Evidence store holds the encrypted payload row.
        let rehydrated = with_runtime(handle, |rt| {
            Ok(rt
                .store()
                .load_approved_document_payload(scope, doc_id_uuid)
                .expect("load_approved_document_payload"))
        })
        .expect("with_runtime");
        assert_eq!(rehydrated, Some(payload.clone()));

        // list_approved_documents returns the joined view.
        let listed =
            list_approved_documents(handle, scope_str.clone()).expect("list_approved_documents");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, summary.id);
        assert_eq!(listed[0].payload_bytes, payload.len() as u64);
        assert_eq!(listed[0].content_hash_hex, summary.content_hash_hex);

        teardown(handle);
    }

    /// Oversize payload is rejected at the FFI boundary with
    /// [`FfiError::Memory`] whose message names both the offending
    /// size and the cap. No row is written, no ref is admitted.
    #[test]
    fn admit_approved_document_oversize_rejected_with_descriptive_error() {
        let (handle, _dir) = fresh_store();
        let scope = seed_tenant_with_domain(handle);
        let scope_str = scope.as_uuid().to_string();

        let oversize = vec![0u8; MAX_APPROVED_DOCUMENT_BYTES + 1];
        let err = admit_approved_document(
            handle,
            scope_str.clone(),
            "label".into(),
            "approver".into(),
            oversize,
        )
        .expect_err("oversize must reject");
        match err {
            FfiError::Memory { message } => {
                assert!(
                    message.contains(&(MAX_APPROVED_DOCUMENT_BYTES + 1).to_string()),
                    "error message must surface the offending size, got: {message}",
                );
                assert!(
                    message.contains(&MAX_APPROVED_DOCUMENT_BYTES.to_string()),
                    "error message must surface the cap, got: {message}",
                );
            }
            other => panic!("expected FfiError::Memory, got {other:?}"),
        }

        // No ref admitted.
        let listed = list_approved_documents(handle, scope_str).expect("list");
        assert!(listed.is_empty(), "no ref must be admitted on oversize");

        teardown(handle);
    }

    /// Empty payload / empty metadata strings are rejected
    /// individually.
    #[test]
    fn admit_approved_document_rejects_empty_inputs() {
        let (handle, _dir) = fresh_store();
        let scope = seed_tenant_with_domain(handle);
        let scope_str = scope.as_uuid().to_string();

        for (label, approver, payload, expect_field) in [
            (
                String::new(),
                "approver".to_string(),
                b"x".to_vec(),
                "label",
            ),
            (
                "label".to_string(),
                String::new(),
                b"x".to_vec(),
                "approver",
            ),
            (
                "label".to_string(),
                "approver".to_string(),
                vec![],
                "payload",
            ),
        ] {
            let err = admit_approved_document(handle, scope_str.clone(), label, approver, payload)
                .expect_err("empty input must reject");
            match err {
                FfiError::Memory { message } => assert!(
                    message.contains(expect_field),
                    "expected field `{expect_field}` in error, got: {message}",
                ),
                other => panic!("expected FfiError::Memory, got {other:?}"),
            }
        }

        teardown(handle);
    }

    /// Revoking a previously admitted document removes both the
    /// tenant-memory ref and the persisted payload row.
    #[test]
    fn revoke_approved_document_purges_ref_and_payload_row() {
        let (handle, _dir) = fresh_store();
        let scope = seed_tenant_with_domain(handle);
        let scope_str = scope.as_uuid().to_string();

        let summary = admit_approved_document(
            handle,
            scope_str.clone(),
            "Tenant Policy".into(),
            "approver".into(),
            b"payload".to_vec(),
        )
        .expect("admit");

        revoke_approved_document(handle, scope_str.clone(), summary.id.clone()).expect("revoke");

        // Tenant memory ref is gone.
        with_runtime(handle, |rt| {
            let tmo = rt.tenant_memory(scope).expect("tenant memory present");
            assert!(tmo.approved_documents.is_empty());
            Ok(())
        })
        .expect("with_runtime");

        // Payload row is gone.
        let payload_after = with_runtime(handle, |rt| {
            Ok(rt
                .store()
                .load_approved_document_payload(scope, Uuid::parse_str(&summary.id).unwrap())
                .unwrap())
        })
        .expect("with_runtime");
        assert!(payload_after.is_none());

        // Listing is empty.
        let listed = list_approved_documents(handle, scope_str.clone()).expect("list");
        assert!(listed.is_empty());

        // Second revoke is NotFound (the ref is gone).
        let err = revoke_approved_document(handle, scope_str, summary.id).unwrap_err();
        assert!(matches!(err, FfiError::NotFound { .. }));

        teardown(handle);
    }

    /// Tenant synthesis materialises the per-tenant payload bundle
    /// from the evidence store under the Phase-1 gather lock. The
    /// stub `ManagedEndpointSynthesizer` concatenates
    /// `doc:<payload>` for every supplied [`ApprovedDocument`], so we
    /// can assert the payload bytes appear verbatim in the resulting
    /// `SynthesisObject.payload`.
    #[test]
    fn trigger_server_synthesis_tenant_sends_approved_documents() {
        let (handle, _dir) = fresh_store();
        install_test_engine(handle);
        let scope = seed_tenant_with_domain(handle);
        let scope_str = scope.as_uuid().to_string();

        let payload_bytes = b"OFFICIAL CHARTER payload bytes".to_vec();
        admit_approved_document(
            handle,
            scope_str.clone(),
            "Charter".into(),
            "approver".into(),
            payload_bytes.clone(),
        )
        .expect("admit");

        let window_id_str =
            trigger_server_synthesis(handle, scope_str.clone(), SynthesisTierKind::Tenant)
                .expect("tenant synthesis");
        let rec = synthesis_status(handle, window_id_str.clone()).expect("status");
        assert_eq!(rec.status, "complete");

        // The persisted synthesis object's payload should contain
        // the document bytes (stub format: `doc:<payload>`).
        let window_uuid: Uuid = window_id_str.parse().unwrap();
        let synth_payload = with_runtime(handle, |rt| {
            let obj = rt
                .synthesis_objects
                .get(&synthesis_pipeline::WindowId::from_uuid(window_uuid))
                .expect("synthesis object present");
            Ok(obj.payload.clone())
        })
        .expect("with_runtime");
        let payload_str = String::from_utf8_lossy(&synth_payload);
        assert!(
            payload_str.contains("doc:OFFICIAL CHARTER payload bytes"),
            "stub-synthesised payload must include the approved-document bytes; got: {payload_str:?}",
        );

        teardown(handle);
    }

    /// A tenant-memory ref without a persisted payload row is
    /// skipped with a warning (the dispatch must NOT fail). This
    /// guards the graceful degradation path: legacy refs or
    /// out-of-band purges should not break the dispatch contract.
    #[test]
    fn trigger_server_synthesis_tenant_tolerates_missing_payload_row() {
        let (handle, _dir) = fresh_store();
        install_test_engine(handle);
        let scope = seed_tenant_with_domain(handle);

        // Synthesise an orphan ref by mutating the tenant memory
        // directly — no `save_approved_document_payload` call.
        let orphan_id = Uuid::new_v4();
        with_runtime(handle, |rt| {
            let mut tmo = rt
                .tenant_memory(scope)
                .cloned()
                .expect("tenant memory present");
            tmo.admit_approved_document(memory_manager::ApprovedDocumentRef {
                id: orphan_id,
                label: "orphan".into(),
                approver: "approver".into(),
                approved_at: Utc::now(),
            });
            rt.save_tenant_memory(scope, tmo)?;
            Ok(())
        })
        .expect("with_runtime");

        // Dispatch must succeed.
        let window_id_str = trigger_server_synthesis(
            handle,
            scope.as_uuid().to_string(),
            SynthesisTierKind::Tenant,
        )
        .expect("tenant synthesis with orphan ref must succeed");
        let rec = synthesis_status(handle, window_id_str).expect("status");
        assert_eq!(rec.status, "complete");

        teardown(handle);
    }

    /// `forget_scope_state` purges every approved-document payload
    /// row bound to the forgotten scope. We invoke
    /// [`crate::forget_scope`] (the public entry point) to make sure
    /// the integration is wired end-to-end, not just the inner
    /// helper.
    #[test]
    fn forget_scope_wipes_approved_document_payloads() {
        let (handle, _dir) = fresh_store();
        let scope = seed_tenant_with_domain(handle);
        let scope_str = scope.as_uuid().to_string();

        let summary_a = admit_approved_document(
            handle,
            scope_str.clone(),
            "A".into(),
            "ap".into(),
            b"a".to_vec(),
        )
        .expect("admit A");
        let summary_b = admit_approved_document(
            handle,
            scope_str.clone(),
            "B".into(),
            "ap".into(),
            b"b".to_vec(),
        )
        .expect("admit B");

        // Sanity-check pre-forget metadata count.
        let metas_before = with_runtime(handle, |rt| {
            Ok(rt
                .store()
                .list_approved_document_payload_meta_for_scope(scope)
                .unwrap())
        })
        .expect("with_runtime");
        assert_eq!(metas_before.len(), 2);

        crate::forget_scope(handle, scope_str.clone()).expect("forget_scope");

        let metas_after = with_runtime(handle, |rt| {
            Ok(rt
                .store()
                .list_approved_document_payload_meta_for_scope(scope)
                .unwrap())
        })
        .expect("with_runtime");
        assert!(
            metas_after.is_empty(),
            "all approved-document payload rows must be purged by forget_scope; \
             survivors: {metas_after:?} (admitted summaries: {summary_a:?}, {summary_b:?})",
        );

        teardown(handle);
    }

    /// Admit / list / revoke calls on a forgotten scope behave per
    /// the documented contract: admit returns `NotFound`, list
    /// returns an empty Vec (soft-empty), revoke returns
    /// `NotFound`.
    #[test]
    fn approved_document_calls_on_forgotten_scope() {
        let (handle, _dir) = fresh_store();
        let scope = seed_tenant_with_domain(handle);
        let scope_str = scope.as_uuid().to_string();
        crate::forget_scope(handle, scope_str.clone()).expect("forget_scope");

        let admit_err = admit_approved_document(
            handle,
            scope_str.clone(),
            "label".into(),
            "approver".into(),
            b"payload".to_vec(),
        )
        .unwrap_err();
        assert!(matches!(admit_err, FfiError::NotFound { ref kind, .. } if kind == "scope"));

        let listed =
            list_approved_documents(handle, scope_str.clone()).expect("list on forgotten scope");
        assert!(
            listed.is_empty(),
            "list on a forgotten scope must be soft-empty",
        );

        let revoke_err =
            revoke_approved_document(handle, scope_str, Uuid::new_v4().to_string()).unwrap_err();
        assert!(matches!(revoke_err, FfiError::NotFound { ref kind, .. } if kind == "scope"));

        teardown(handle);
    }
}
