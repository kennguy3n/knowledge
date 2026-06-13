//! Internal HTTP loopback server exposing the Knowledge substrate's
//! Rust FFI surface to the Go server tier.
//!
//! Rust stays the library; Go is the server. This binary crate is the
//! thin bridge between them: it boots an `axum` 0.8 service on
//! `127.0.0.1:9090` (loopback only — never exposed publicly) that
//! wraps each relevant `ffi::*` function behind a REST endpoint, plus
//! a handful of endpoints that call the `permission_service`,
//! `export_plane`, and `crypto` crates directly where no FFI function
//! exists.
//!
//! The FFI functions are synchronous and take a `RuntimeHandle` as
//! their first argument; handlers dispatch them on the blocking
//! thread pool via [`tokio::task::spawn_blocking`] so the async
//! runtime is never stalled by a SQLCipher round-trip.

pub mod config;
pub mod dto;
pub mod error;
pub mod key_rotation;
pub mod metrics;
pub mod replication;
pub mod state;
pub mod update_check;

use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use export_plane::{ExportDecision, ExportPolicy, PolicyEngine, PortableConceptProfile};
use ffi::{
    ConnectorHealthRecord, ConnectorStatus, ContradictionView, DriftView, EvidenceRecord, FfiError,
    FfiKeypair, GraphView, HealthStatus, MemoryRecord, QueryExplanationView, QueryResult,
    RuntimeHandle, SyncReport, SynthesisStatusRecord, SynthesisTierKind,
};
use permission_service::{check_permission, RelationTuple};
use serde::{Deserialize, Serialize};

use crate::dto::{
    AddUserMemoryRequest, AuthenticateRequest, CreateConnectorRequest, ExplainQueryRequest,
    FetchContentRequest, ForgetScopeRequest, IdRequest, IdResponse, IngestRequest,
    ListMemoriesRequest, QueryRequest, ReasoningScopeRequest, RecentSynthesisRequest,
    ServerSynthesisRequest, SynthesisTriggerRequest,
};
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// Run an infallible-on-the-happy-path synchronous FFI call on the
/// blocking thread pool and adapt its `FfiResult` into an
/// [`ApiResult`].
async fn blocking<F, T>(f: F) -> ApiResult<T>
where
    F: FnOnce() -> ffi::FfiResult<T> + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(inner) => inner.map_err(ApiError::from),
        Err(join) => Err(ApiError(FfiError::Unavailable {
            subsystem: format!("blocking-pool join failure: {join}"),
        })),
    }
}

/// Reject a mutating request when this node is not the current primary.
///
/// On a standalone substrate (`replication.enabled = false`) the node
/// is always writable, so this is a no-op. Under active-passive
/// replication a standby returns `503 Service Unavailable` with a
/// `replication-standby` subsystem marker; the Go gateway treats that
/// status as "primary moved" and retries the write against the node
/// that currently reports `role = primary` (see
/// `server/internal/substrate`). Mapping to `503` (rather than a 4xx)
/// keeps the failure transient/retriable from every HTTP client's
/// perspective.
fn guard_writable(st: &AppState) -> ApiResult<()> {
    replication::failover::ensure_writable(&st.replication).map_err(|e| {
        ApiError(FfiError::Unavailable {
            subsystem: format!("replication-standby: {e}"),
        })
    })
}

// ───────────────────────────── Evidence ─────────────────────────────

/// `POST /ingest` — persist a message into the encrypted evidence
/// plane. Returns the new row's UUID.
async fn ingest(
    State(st): State<AppState>,
    Json(req): Json<IngestRequest>,
) -> ApiResult<Json<IdResponse>> {
    guard_writable(&st)?;
    let handle = st.handle;
    let id = blocking(move || {
        ffi::ingest_message(handle, req.scope_id, req.body, req.source, req.importance)
    })
    .await?;
    Ok(Json(IdResponse { id }))
}

/// `POST /query` — hybrid FTS query against a scope.
async fn query(
    State(st): State<AppState>,
    Json(req): Json<QueryRequest>,
) -> ApiResult<Json<Vec<QueryResult>>> {
    let handle = st.handle;
    let rows =
        blocking(move || ffi::query(handle, req.scope_id, req.query_text, req.limit)).await?;
    Ok(Json(rows))
}

/// `GET /evidence/{id}` — fetch one decrypted evidence row.
async fn get_evidence(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<EvidenceRecord>> {
    let handle = st.handle;
    let row = blocking(move || ffi::get_evidence(handle, id)).await?;
    Ok(Json(row))
}

/// `POST /memories` — list per-user memories for a scope.
async fn list_memories(
    State(st): State<AppState>,
    Json(req): Json<ListMemoriesRequest>,
) -> ApiResult<Json<Vec<MemoryRecord>>> {
    let handle = st.handle;
    let rows = blocking(move || ffi::list_memories(handle, req.scope_id, req.filter)).await?;
    Ok(Json(rows))
}

/// `POST /user_memory` — create a new user-memory observation for a
/// scope and return the created [`MemoryRecord`].
///
/// This is the write counterpart to `POST /memories` (list) and the
/// only caller-facing path that populates per-user memory: synthesis
/// owns the channel / domain / tenant tiers, so they have no write
/// route here. The mutation is guarded by [`guard_writable`] so a
/// replication standby returns `503` and the Go client retries the
/// write against the primary.
async fn add_user_memory(
    State(st): State<AppState>,
    Json(req): Json<AddUserMemoryRequest>,
) -> ApiResult<(StatusCode, Json<MemoryRecord>)> {
    guard_writable(&st)?;
    let handle = st.handle;
    let record = blocking(move || {
        ffi::add_user_memory(
            handle,
            req.scope_id,
            req.observation_type,
            req.content,
            req.sensitivity,
        )
    })
    .await?;
    Ok((StatusCode::CREATED, Json(record)))
}

/// `GET /channel_memory/{scope_id}` — latest synthesised channel
/// recap for a scope.
///
/// Returns the [`MemoryRecord`] holding the recap produced by the
/// most recent [`ffi::trigger_synthesis`] run, or `404 Not Found`
/// when synthesis has never produced a recap for the scope (or the
/// scope has been forgotten). This is the read side of the synthesis
/// surface: `POST /synthesis/trigger` writes the recap into the
/// channel memory object, and this route reads it back — the
/// per-window `/synthesis/{id}/status` + `/synthesis/recent`
/// endpoints only report lifecycle metadata, not the recap text.
async fn get_channel_memory(
    State(st): State<AppState>,
    Path(scope_id): Path<String>,
) -> ApiResult<Json<MemoryRecord>> {
    let handle = st.handle;
    let id_for_err = scope_id.clone();
    let memory = blocking(move || ffi::get_channel_memory(handle, scope_id)).await?;
    match memory {
        Some(record) => Ok(Json(record)),
        None => Err(ApiError(FfiError::NotFound {
            kind: "channel_memory".into(),
            id: id_for_err,
        })),
    }
}

/// `GET /concept_graph/{scope_id}` — the per-scope concept graph
/// projected from the scope's live user-memory observations.
///
/// This is a pure read (it routes to a replica like the other `GET`
/// endpoints): [`ffi::get_concept_graph`] derives the
/// [`GraphView`] on the fly from the same per-scope memory the decay
/// sweep mutates, so the graph the UI renders can never disagree with
/// the memory list. A scope with no memory — or a forgotten scope —
/// yields an empty graph (`200` with empty `nodes`/`edges`), not a
/// `404`: an empty graph is a valid, honest state the UI renders as
/// "no concepts yet" rather than an error.
async fn get_concept_graph(
    State(st): State<AppState>,
    Path(scope_id): Path<String>,
) -> ApiResult<Json<GraphView>> {
    let handle = st.handle;
    let view = blocking(move || ffi::get_concept_graph(handle, scope_id)).await?;
    Ok(Json(view))
}

/// `POST /reasoning/contradictions` — opposing canonical claims in a
/// scope (the *"what contradicts"* surface).
///
/// Like `/concept_graph`, this is a pure read routed to a replica:
/// [`ffi::reasoning_contradictions`] projects the concept graph from
/// *only* the scope's live user memory and scans it for opposing
/// canonical claims. A scope with no contradictions — or a forgotten
/// scope — yields an empty list (`200` with `[]`), never a `404`.
async fn reasoning_contradictions(
    State(st): State<AppState>,
    Json(req): Json<ReasoningScopeRequest>,
) -> ApiResult<Json<Vec<ContradictionView>>> {
    let handle = st.handle;
    let rows = blocking(move || ffi::reasoning_contradictions(handle, req.scope_id)).await?;
    Ok(Json(rows))
}

/// `POST /reasoning/drift` — canonical claims whose evidence base has
/// shifted in a scope (the *"what changed"* surface).
///
/// A pure read with the same scope-isolation and empty-is-valid
/// semantics as `/reasoning/contradictions`.
async fn reasoning_drift(
    State(st): State<AppState>,
    Json(req): Json<ReasoningScopeRequest>,
) -> ApiResult<Json<Vec<DriftView>>> {
    let handle = st.handle;
    let rows = blocking(move || ffi::reasoning_drift(handle, req.scope_id)).await?;
    Ok(Json(rows))
}

/// `POST /reasoning/explain` — the query planner's rationale for a
/// retrieval (the *"why this answer"* surface).
///
/// The plan is a pure function of the query text — it reads no scope
/// data — so it touches neither the primary nor a replica's row data;
/// it still validates the scope id so the authorisation envelope is
/// uniform across the reasoning routes.
async fn reasoning_explain(
    Json(req): Json<ExplainQueryRequest>,
) -> ApiResult<Json<QueryExplanationView>> {
    let view = blocking(move || ffi::reasoning_explain_query(req.scope_id, req.query)).await?;
    Ok(Json(view))
}

/// `POST /pin` — mark a memory decay-immune.
async fn pin(State(st): State<AppState>, Json(req): Json<IdRequest>) -> ApiResult<StatusCode> {
    guard_writable(&st)?;
    let handle = st.handle;
    blocking(move || ffi::pin(handle, req.id)).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /unpin` — release a pin.
async fn unpin(State(st): State<AppState>, Json(req): Json<IdRequest>) -> ApiResult<StatusCode> {
    guard_writable(&st)?;
    let handle = st.handle;
    blocking(move || ffi::unpin(handle, req.id)).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /forget` — cryptographically forget a single evidence row.
async fn forget(State(st): State<AppState>, Json(req): Json<IdRequest>) -> ApiResult<StatusCode> {
    guard_writable(&st)?;
    let handle = st.handle;
    blocking(move || ffi::forget(handle, req.id)).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /forget_scope` — cryptographically forget an entire scope.
async fn forget_scope(
    State(st): State<AppState>,
    Json(req): Json<ForgetScopeRequest>,
) -> ApiResult<StatusCode> {
    guard_writable(&st)?;
    let handle = st.handle;
    blocking(move || ffi::forget_scope(handle, req.scope_id)).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ──────────────────────────── Synthesis ─────────────────────────────

/// `POST /synthesis/trigger` — kick off a synthesis cycle. Returns
/// the new synthesis window's UUID.
async fn synthesis_trigger(
    State(st): State<AppState>,
    Json(req): Json<SynthesisTriggerRequest>,
) -> ApiResult<Json<IdResponse>> {
    guard_writable(&st)?;
    let handle = st.handle;
    let id = blocking(move || ffi::trigger_synthesis(handle, req.scope_id, req.trigger)).await?;
    Ok(Json(IdResponse { id }))
}

/// `POST /synthesis/domain` — roll up a domain's registered channel
/// outputs into a `DomainSummary`. Returns the new synthesis window's
/// UUID, pollable via `GET /synthesis/{id}/status`.
///
/// This is the server-side tier of the synthesis hierarchy: it
/// dispatches the FFI [`ffi::trigger_server_synthesis`] entry point
/// (which invokes `synthesis_engine`'s `synthesize_domain`) under the
/// same gather → dispatch → apply locking discipline as the channel
/// [`synthesis_trigger`] path. Guarded by [`guard_writable`] so a
/// replication standby rejects it with `503` and the Go client retries
/// against the primary.
async fn synthesis_domain(
    State(st): State<AppState>,
    Json(req): Json<ServerSynthesisRequest>,
) -> ApiResult<Json<IdResponse>> {
    guard_writable(&st)?;
    let handle = st.handle;
    let id = blocking(move || {
        ffi::trigger_server_synthesis(handle, req.scope_id, SynthesisTierKind::Domain)
    })
    .await?;
    Ok(Json(IdResponse { id }))
}

/// `POST /synthesis/tenant` — roll up a tenant's registered domain
/// outputs plus approved documents into a `TenantSummary`. Returns the
/// new synthesis window's UUID, pollable via `GET /synthesis/{id}/status`.
///
/// The tenant-tier counterpart to [`synthesis_domain`]; it invokes
/// `synthesis_engine`'s `synthesize_tenant` via
/// [`ffi::trigger_server_synthesis`]. Same writable guard applies.
async fn synthesis_tenant(
    State(st): State<AppState>,
    Json(req): Json<ServerSynthesisRequest>,
) -> ApiResult<Json<IdResponse>> {
    guard_writable(&st)?;
    let handle = st.handle;
    let id = blocking(move || {
        ffi::trigger_server_synthesis(handle, req.scope_id, SynthesisTierKind::Tenant)
    })
    .await?;
    Ok(Json(IdResponse { id }))
}

/// `GET /synthesis/{id}/status` — current status of a synthesis
/// window.
async fn synthesis_status(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<SynthesisStatusRecord>> {
    let handle = st.handle;
    let rec = blocking(move || ffi::synthesis_status(handle, id)).await?;
    Ok(Json(rec))
}

/// `POST /synthesis/recent` — recent synthesis windows for a scope.
async fn synthesis_recent(
    State(st): State<AppState>,
    Json(req): Json<RecentSynthesisRequest>,
) -> ApiResult<Json<Vec<SynthesisStatusRecord>>> {
    let handle = st.handle;
    let rows = blocking(move || ffi::list_recent_syntheses(handle, req.scope_id)).await?;
    Ok(Json(rows))
}

// ──────────────────────────── Connectors ────────────────────────────

/// `POST /connectors` — register a connector instance.
async fn create_connector(
    State(st): State<AppState>,
    Json(req): Json<CreateConnectorRequest>,
) -> ApiResult<Json<IdResponse>> {
    guard_writable(&st)?;
    let handle = st.handle;
    let id =
        blocking(move || ffi::create_connector(handle, req.kind, req.scope_id, req.config_json))
            .await?;
    Ok(Json(IdResponse { id }))
}

/// `GET /connectors` — list all connector instances.
async fn list_connectors(State(st): State<AppState>) -> ApiResult<Json<Vec<ConnectorStatus>>> {
    let handle = st.handle;
    let rows = blocking(move || ffi::list_connectors(handle)).await?;
    Ok(Json(rows))
}

/// `POST /connectors/{id}/authenticate` — complete the OAuth2
/// code-exchange for a connector instance.
async fn authenticate_connector(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<AuthenticateRequest>,
) -> ApiResult<StatusCode> {
    guard_writable(&st)?;
    let handle = st.handle;
    blocking(move || ffi::authenticate_connector(handle, id, req.auth_code)).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /connectors/{id}/sync` — run an incremental sync.
async fn sync_connector(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<SyncReport>> {
    guard_writable(&st)?;
    let handle = st.handle;
    let report = blocking(move || ffi::sync_connector(handle, id)).await?;
    Ok(Json(report))
}

/// `DELETE /connectors/{id}` — remove a connector instance.
async fn remove_connector(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    guard_writable(&st)?;
    let handle = st.handle;
    blocking(move || ffi::remove_connector(handle, id)).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /connectors/{id}/status` — current status of one connector.
async fn connector_status(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<ConnectorHealthRecord>> {
    let handle = st.handle;
    let status = blocking(move || ffi::connector_status(handle, id)).await?;
    Ok(Json(status))
}

/// `POST /connector/fetch_content` — the provider content-fetch
/// trait method is not wired into this server build, so the endpoint
/// returns `501 Not Implemented`. The Go connector pipeline treats
/// the feature as "not yet available" and falls back to a mock.
async fn fetch_content(Json(_req): Json<FetchContentRequest>) -> ApiError {
    ApiError(FfiError::Unimplemented {
        method: "fetch_content".to_string(),
    })
}

// ──────────────────────────── Permission ────────────────────────────

/// `{ "allowed": bool }` response from a permission check.
#[derive(Debug, Clone, Serialize)]
struct PermissionCheckResponse {
    /// Whether the `(subject, relation, object)` is authorised.
    allowed: bool,
}

/// `POST /permission/grant` — idempotently insert a relation tuple.
async fn permission_grant(
    State(st): State<AppState>,
    Json(tuple): Json<RelationTuple>,
) -> ApiResult<StatusCode> {
    guard_writable(&st)?;
    let mut guard = st.permissions.lock().map_err(|_| permission_poisoned())?;
    let inserted = guard.store.upsert(tuple).map_err(map_permission_err)?;
    // Idempotent: a repeat grant is a no-op `200`; a fresh grant is
    // `201 Created`.
    Ok(if inserted {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    })
}

/// `POST /permission/revoke` — remove a relation tuple. Returns `404`
/// if the tuple was not present.
async fn permission_revoke(
    State(st): State<AppState>,
    Json(tuple): Json<RelationTuple>,
) -> ApiResult<StatusCode> {
    guard_writable(&st)?;
    let mut guard = st.permissions.lock().map_err(|_| permission_poisoned())?;
    if !guard.store.store().contains(&tuple) {
        return Err(ApiError(FfiError::NotFound {
            kind: "relation_tuple".to_string(),
            id: format!("{:?}", tuple.relation),
        }));
    }
    guard.store.remove(&tuple).map_err(map_permission_err)?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /permission/check` — evaluate a `(subject, relation, object)`
/// authorisation query against the tuple set + namespace closure.
async fn permission_check(
    State(st): State<AppState>,
    Json(tuple): Json<RelationTuple>,
) -> ApiResult<Json<PermissionCheckResponse>> {
    let guard = st.permissions.lock().map_err(|_| permission_poisoned())?;
    let allowed = check_permission(
        guard.store.store(),
        &guard.namespaces,
        tuple.object,
        tuple.relation,
        tuple.subject,
    );
    Ok(Json(PermissionCheckResponse { allowed }))
}

/// Construct the error returned when the permission mutex is
/// poisoned (a previous handler panicked while holding it).
fn permission_poisoned() -> ApiError {
    ApiError(FfiError::Unavailable {
        subsystem: "permission-store (mutex poisoned)".to_string(),
    })
}

/// Map a [`permission_service::PermissionError`] (raised by a
/// persistent-store mutation) onto the wire error type. `NotFound`
/// becomes a `404`; everything else is a persistence/`Unavailable`
/// failure of the permission subsystem.
fn map_permission_err(e: permission_service::PermissionError) -> ApiError {
    match e {
        permission_service::PermissionError::NotFound => ApiError(FfiError::NotFound {
            kind: "relation_tuple".to_string(),
            id: String::new(),
        }),
        other => ApiError(FfiError::Unavailable {
            subsystem: format!("permission-store: {other}"),
        }),
    }
}

// ────────────────────────────── Crypto ──────────────────────────────

/// Response from `POST /crypto/hybrid_keypair`.
///
/// **Loopback only.** `secret_key_hex` carries private key material;
/// it is returned to the Go tenant service for per-tenant key
/// management and MUST NOT be logged or exposed beyond the internal
/// loopback boundary.
struct HybridKeypairResponse {
    /// Algorithm tag: classical + post-quantum hybrid.
    algorithm: String,
    /// Hex of `x25519 (32B) || ML-KEM-768 (1184B)` public key.
    public_key_hex: String,
    /// Hex of `x25519 (32B) || ML-KEM-768 (2400B)` secret key.
    ///
    /// Wrapped in [`zeroize::Zeroizing`] so the hex of the private key
    /// is wiped from the heap when this response drops (immediately
    /// after the loopback body is serialised) instead of lingering in
    /// the allocator's freelist. No `Debug`/`Clone` is derived, to
    /// avoid accidentally copying or logging the secret material.
    secret_key_hex: zeroize::Zeroizing<String>,
}

impl Serialize for HybridKeypairResponse {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        let mut st = serializer.serialize_struct("HybridKeypairResponse", 3)?;
        st.serialize_field("algorithm", &self.algorithm)?;
        st.serialize_field("public_key_hex", &self.public_key_hex)?;
        st.serialize_field("secret_key_hex", self.secret_key_hex.as_str())?;
        st.end()
    }
}

/// `POST /crypto/hybrid_keypair` — generate a fresh hybrid KEM
/// keypair (X25519 + ML-KEM-768) for per-tenant encryption-key
/// management.
async fn hybrid_keypair() -> ApiResult<Json<HybridKeypairResponse>> {
    let (pk, sk) = crypto::hybrid_keypair().map_err(|e| {
        ApiError(FfiError::Crypto {
            message: e.to_string(),
        })
    })?;
    let mut public = Vec::with_capacity(pk.x25519.len() + pk.mlkem768.len());
    public.extend_from_slice(&pk.x25519);
    public.extend_from_slice(&pk.mlkem768);
    // Hold the raw secret bytes in `Zeroizing` so the concatenated
    // private key is wiped from the heap when this handler returns.
    let mut secret =
        zeroize::Zeroizing::new(Vec::with_capacity(sk.x25519.len() + sk.mlkem768.len()));
    secret.extend_from_slice(&sk.x25519);
    secret.extend_from_slice(&sk.mlkem768);
    Ok(Json(HybridKeypairResponse {
        algorithm: "x25519+ml-kem-768".to_string(),
        public_key_hex: to_hex(&public),
        secret_key_hex: zeroize::Zeroizing::new(to_hex(secret.as_slice())),
    }))
}

/// `POST /crypto/signing_keypair` — generate an ML-DSA-65 signing
/// keypair via the FFI surface.
async fn signing_keypair() -> ApiResult<Json<FfiKeypair>> {
    let kp = blocking(ffi::generate_keypair).await?;
    Ok(Json(kp))
}

/// Lower-case hex encoding without pulling in a `hex` dependency.
fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[usize::from(b >> 4)] as char);
        out.push(HEX[usize::from(b & 0x0f)] as char);
    }
    out
}

// ────────────────────────────── Export ──────────────────────────────

/// `POST /export/evaluate` body: a portable concept profile plus an
/// optional base policy. The profile's own constraints are folded
/// into the policy before evaluation (constraints only ever tighten).
#[derive(Debug, Clone, Deserialize)]
struct ExportEvaluateRequest {
    /// Optional base policy; defaults to the most restrictive policy.
    #[serde(default)]
    policy: Option<ExportPolicy>,
    /// The profile to evaluate.
    profile: PortableConceptProfile,
}

/// `POST /export/evaluate` — run a portable concept profile through
/// the export policy engine and return the approve/reject decision.
async fn export_evaluate(
    Json(req): Json<ExportEvaluateRequest>,
) -> ApiResult<Json<ExportDecision>> {
    let policy = req
        .policy
        .unwrap_or_default()
        .with_constraints(&req.profile.constraints);
    let decision = PolicyEngine::new().evaluate(&policy, &req.profile.concepts);
    Ok(Json(decision))
}

// ────────────────────────── Health / metrics ────────────────────────

/// `GET /health` — probe every subsystem reachable through the FFI
/// runtime, augmented with the node's replication status.
///
/// The base [`HealthStatus`] is serialised and a `replication` object
/// (`{ enabled, role, lag_frames, last_applied_at, … }`) is spliced in
/// so the Go gateway can surface failover state without a second call.
async fn health(State(st): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    let handle = st.handle;
    let status: HealthStatus = blocking(move || ffi::health_check(Some(handle))).await?;
    let mut value = serde_json::to_value(&status).map_err(|e| {
        ApiError(FfiError::Unavailable {
            subsystem: format!("serialising health status: {e}"),
        })
    })?;
    if let serde_json::Value::Object(map) = &mut value {
        let repl =
            serde_json::to_value(st.replication.snapshot()).unwrap_or(serde_json::Value::Null);
        map.insert("replication".to_string(), repl);
    }
    Ok(Json(value))
}

/// `GET /internal/metrics` — Prometheus text exposition built from
/// `ffi::metrics::snapshot()`, with the replication gauges and the
/// substrate's latency histograms (`knowledge_open_store_duration_seconds`
/// and the per-`(task, adapter)` `knowledge_slm_dispatch_duration_seconds`)
/// appended.
async fn internal_metrics(State(st): State<AppState>) -> impl IntoResponse {
    let snapshot = ffi::metrics_snapshot();
    let mut body = metrics::render(&snapshot);
    body.push_str(&metrics::render_replication(&st.replication.snapshot()));

    // Append the latency histograms. The SLM dispatch histogram is
    // per-runtime (it lives on the runtime's inference router), so it
    // needs the handle; the open-store histogram is process-global. A
    // closed/unknown handle just yields no SLM series rather than
    // failing the scrape.
    let handle = st.handle;
    let open_store = ffi::open_store_duration_histogram();
    let slm = blocking(move || ffi::slm_dispatch_histograms(handle))
        .await
        .unwrap_or_default();
    body.push_str(&metrics::render_histograms(&open_store, &slm));

    (
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        body,
    )
}

/// Build the full axum [`Router`] with state attached.
///
/// Exposed so integration tests can exercise every route in-process
/// (via `tower::ServiceExt::oneshot` or `axum_test`) without binding
/// a real socket.
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/internal/metrics", get(internal_metrics))
        .route(
            "/internal/update_check",
            get(update_check::update_check_handler),
        )
        .route("/ingest", post(ingest))
        .route("/query", post(query))
        .route("/evidence/{id}", get(get_evidence))
        .route("/memories", post(list_memories))
        .route("/user_memory", post(add_user_memory))
        .route("/channel_memory/{scope_id}", get(get_channel_memory))
        .route("/concept_graph/{scope_id}", get(get_concept_graph))
        .route("/reasoning/contradictions", post(reasoning_contradictions))
        .route("/reasoning/drift", post(reasoning_drift))
        .route("/reasoning/explain", post(reasoning_explain))
        .route("/pin", post(pin))
        .route("/unpin", post(unpin))
        .route("/forget", post(forget))
        .route("/forget_scope", post(forget_scope))
        .route("/synthesis/trigger", post(synthesis_trigger))
        .route("/synthesis/domain", post(synthesis_domain))
        .route("/synthesis/tenant", post(synthesis_tenant))
        .route("/synthesis/{id}/status", get(synthesis_status))
        .route("/synthesis/recent", post(synthesis_recent))
        .route("/connectors", post(create_connector).get(list_connectors))
        .route(
            "/connectors/{id}/authenticate",
            post(authenticate_connector),
        )
        .route("/connectors/{id}/sync", post(sync_connector))
        .route("/connectors/{id}", delete(remove_connector))
        .route("/connectors/{id}/status", get(connector_status))
        .route("/connector/fetch_content", post(fetch_content))
        .route("/permission/grant", post(permission_grant))
        .route("/permission/revoke", post(permission_revoke))
        .route("/permission/check", post(permission_check))
        .route("/crypto/hybrid_keypair", post(hybrid_keypair))
        .route("/crypto/signing_keypair", post(signing_keypair))
        .route("/export/evaluate", post(export_evaluate))
        .with_state(state)
}

/// Open the evidence store from `config` and return the runtime
/// handle. Separated from [`run`] so tests can open a temp store and
/// build the router without binding a socket.
///
/// # Errors
///
/// Propagates any [`FfiError`] from `ffi::open_store` (bad master
/// key, SQLCipher open failure, …).
pub fn open_runtime(config: &config::ServerConfig) -> ffi::FfiResult<RuntimeHandle> {
    ffi::open_store(config.store_path.clone(), config.master_key_hex.to_string())
}

/// Install the server-side synthesis engine on `handle` when
/// [`config::ServerConfig::synthesis`] is set.
///
/// This is what makes the `/synthesis/domain` and `/synthesis/tenant`
/// routes functional: those tiers dispatch through the FFI engine slot
/// populated here (the on-device channel tier does not). When no
/// synthesis config is present this is a no-op and the server-side
/// routes report `503` (engine unavailable) on use.
///
/// Fails fast (propagating the [`ffi::FfiError`]) when a synthesis URL
/// was configured but the engine could not be installed — e.g. a binary
/// built without the `http-client` feature, or a malformed endpoint
/// config. A deployment that opted into server-side synthesis must not
/// silently boot with dead `/synthesis/{domain,tenant}` routes.
///
/// # Errors
///
/// Returns the underlying [`ffi::FfiError`] if
/// [`ffi::configure_synthesis_engine`] rejects the configuration.
pub fn configure_synthesis(
    handle: RuntimeHandle,
    config: &config::ServerConfig,
) -> ffi::FfiResult<()> {
    let Some(settings) = config.synthesis.as_ref() else {
        return Ok(());
    };
    ffi::configure_synthesis_engine(handle, settings.to_ffi())?;
    tracing::info!(
        url = %settings.url,
        model = %settings.model_id,
        single_tenant = settings.single_tenant,
        scope_bound = settings.scope_bindings.is_some(),
        "substrate_server: server-side synthesis engine installed",
    );
    Ok(())
}

/// Boot the loopback server: read config from env, open the store,
/// bind the configured address, and serve until `SIGINT`/`Ctrl-C`.
///
/// # Errors
///
/// Returns a boxed error if config assembly, store open, socket bind,
/// or the server loop fails.
pub async fn run(
    role_override: Option<String>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let config = config::ServerConfig::from_env()?;
    let bind_addr = config.bind_addr;
    let config = std::sync::Arc::new(config);

    // `open_runtime` may build and drop a short-lived Tokio runtime
    // while rehydrating the store, and `configure_synthesis` builds a
    // reqwest blocking client whose internal runtime is likewise dropped
    // during construction. Doing either on this `#[tokio::main]` worker
    // thread trips tokio's "cannot drop a runtime within an async
    // context" guard, which would panic before the server ever binds. So
    // open the store *and* install the synthesis engine on a dedicated
    // thread with no ambient runtime; the returned handle indexes a
    // global registry, so it stays valid back on the async thread.
    //
    // Installing the engine here also fails fast: a deployment that set
    // KNOWLEDGE_SYNTHESIS_URL but cannot install the engine (e.g. a
    // binary built without `http-client`) refuses to boot rather than
    // silently serving `503` on /synthesis/{domain,tenant}.
    let open_cfg = std::sync::Arc::clone(&config);
    let handle = std::thread::spawn(move || -> ffi::FfiResult<RuntimeHandle> {
        let handle = open_runtime(&open_cfg)?;
        configure_synthesis(handle, &open_cfg)?;
        Ok(handle)
    })
    .join()
    .map_err(|_| "substrate_server: store-open thread panicked")??;
    tracing::info!(%bind_addr, "substrate_server: evidence store opened, binding loopback");

    // Resolve replication config (CLI `--role` overrides the env) and,
    // if enabled, start the failover coordinator. The shared state is
    // handed to the router so `/health` and `/internal/metrics` report
    // the live role / lag.
    let repl_config =
        replication::ReplicationConfig::from_env(&config.store_path, role_override.as_deref())?;
    let replication_shared = std::sync::Arc::new(
        if matches!(repl_config.mode, replication::ReplicationMode::Disabled) {
            replication::ReplicationShared::disabled()
        } else {
            replication::ReplicationShared::enabled(repl_config.initial_role())
        },
    );
    let (repl_shutdown_tx, repl_shutdown_rx) = tokio::sync::watch::channel(false);
    let repl_handle = replication::spawn(
        repl_config,
        std::sync::Arc::clone(&replication_shared),
        repl_shutdown_rx,
        // Hand the open store handle to the standby loop so its raw WAL
        // applies serialise against SQLite reads on the same file.
        Some(handle),
    )
    .await?;

    let state =
        AppState::new(handle, config)?.with_replication(std::sync::Arc::clone(&replication_shared));
    let app = build_router(state);

    let listener = tokio::net::TcpListener::bind(bind_addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    // Drain the replication coordinator before exiting so a primary
    // releases its lease promptly (letting a standby promote without
    // waiting out the lease TTL).
    let _ = repl_shutdown_tx.send(true);
    if let Some(handle) = repl_handle {
        let _ = handle.await;
    }
    Ok(())
}

/// Resolve when the process receives `Ctrl-C` (`SIGINT`). Used to
/// drive axum's graceful shutdown.
async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("substrate_server: shutdown signal received, draining");
}
