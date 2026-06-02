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
pub mod metrics;
pub mod state;

use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use export_plane::{ExportDecision, ExportPolicy, PolicyEngine, PortableConceptProfile};
use ffi::{
    ConnectorHealthRecord, ConnectorStatus, EvidenceRecord, FfiError, FfiKeypair, HealthStatus,
    MemoryRecord, QueryResult, RuntimeHandle, SyncReport, SynthesisStatusRecord,
};
use permission_service::{check_permission, RelationTuple};
use serde::{Deserialize, Serialize};

use crate::dto::{
    AuthenticateRequest, CreateConnectorRequest, FetchContentRequest, ForgetScopeRequest,
    IdRequest, IdResponse, IngestRequest, ListMemoriesRequest, QueryRequest,
    RecentSynthesisRequest, SynthesisTriggerRequest,
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

// ───────────────────────────── Evidence ─────────────────────────────

/// `POST /ingest` — persist a message into the encrypted evidence
/// plane. Returns the new row's UUID.
async fn ingest(
    State(st): State<AppState>,
    Json(req): Json<IngestRequest>,
) -> ApiResult<Json<IdResponse>> {
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

/// `POST /pin` — mark a memory decay-immune.
async fn pin(State(st): State<AppState>, Json(req): Json<IdRequest>) -> ApiResult<StatusCode> {
    let handle = st.handle;
    blocking(move || ffi::pin(handle, req.id)).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /unpin` — release a pin.
async fn unpin(State(st): State<AppState>, Json(req): Json<IdRequest>) -> ApiResult<StatusCode> {
    let handle = st.handle;
    blocking(move || ffi::unpin(handle, req.id)).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /forget` — cryptographically forget a single evidence row.
async fn forget(State(st): State<AppState>, Json(req): Json<IdRequest>) -> ApiResult<StatusCode> {
    let handle = st.handle;
    blocking(move || ffi::forget(handle, req.id)).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /forget_scope` — cryptographically forget an entire scope.
async fn forget_scope(
    State(st): State<AppState>,
    Json(req): Json<ForgetScopeRequest>,
) -> ApiResult<StatusCode> {
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
    let handle = st.handle;
    let id = blocking(move || ffi::trigger_synthesis(handle, req.scope_id, req.trigger)).await?;
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
    let handle = st.handle;
    blocking(move || ffi::authenticate_connector(handle, id, req.auth_code)).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /connectors/{id}/sync` — run an incremental sync.
async fn sync_connector(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<SyncReport>> {
    let handle = st.handle;
    let report = blocking(move || ffi::sync_connector(handle, id)).await?;
    Ok(Json(report))
}

/// `DELETE /connectors/{id}` — remove a connector instance.
async fn remove_connector(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
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

/// `POST /connector/fetch_content` — Session B owns the real
/// `fetch_content` trait method + endpoint. Until that lands this
/// returns `501 Not Implemented` so the Go connector pipeline can
/// treat the feature as "not yet available" and fall back to a mock.
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
    let mut guard = st.permissions.lock().map_err(|_| permission_poisoned())?;
    let inserted = guard.store.upsert(tuple);
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
    let mut guard = st.permissions.lock().map_err(|_| permission_poisoned())?;
    if !guard.store.contains(&tuple) {
        return Err(ApiError(FfiError::NotFound {
            kind: "relation_tuple".to_string(),
            id: format!("{:?}", tuple.relation),
        }));
    }
    guard.store.remove(&tuple).map_err(|e| {
        ApiError(FfiError::Evidence {
            message: e.to_string(),
        })
    })?;
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
        &guard.store,
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
/// runtime.
async fn health(State(st): State<AppState>) -> ApiResult<Json<HealthStatus>> {
    let handle = st.handle;
    let status = blocking(move || ffi::health_check(Some(handle))).await?;
    Ok(Json(status))
}

/// `GET /internal/metrics` — Prometheus text exposition built from
/// `ffi::metrics::snapshot()`.
async fn internal_metrics() -> impl IntoResponse {
    let snapshot = ffi::metrics_snapshot();
    (
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        metrics::render(&snapshot),
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
        .route("/ingest", post(ingest))
        .route("/query", post(query))
        .route("/evidence/{id}", get(get_evidence))
        .route("/memories", post(list_memories))
        .route("/pin", post(pin))
        .route("/unpin", post(unpin))
        .route("/forget", post(forget))
        .route("/forget_scope", post(forget_scope))
        .route("/synthesis/trigger", post(synthesis_trigger))
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

/// Boot the loopback server: read config from env, open the store,
/// bind the configured address, and serve until `SIGINT`/`Ctrl-C`.
///
/// # Errors
///
/// Returns a boxed error if config assembly, store open, socket bind,
/// or the server loop fails.
pub async fn run() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let config = config::ServerConfig::from_env()?;
    let bind_addr = config.bind_addr;
    let config = std::sync::Arc::new(config);

    // `open_runtime` may build and drop a short-lived Tokio runtime
    // while rehydrating the store. Doing that on this `#[tokio::main]`
    // worker thread trips tokio's "cannot drop a runtime within an
    // async context" guard, which would panic before the server ever
    // binds. Open on a dedicated thread with no ambient runtime; the
    // returned handle indexes a global registry, so it stays valid
    // back on the async thread.
    let open_cfg = std::sync::Arc::clone(&config);
    let handle = std::thread::spawn(move || open_runtime(&open_cfg))
        .join()
        .map_err(|_| "substrate_server: store-open thread panicked")??;
    tracing::info!(%bind_addr, "substrate_server: evidence store opened, binding loopback");

    let state = AppState::new(handle, config);
    let app = build_router(state);

    let listener = tokio::net::TcpListener::bind(bind_addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

/// Resolve when the process receives `Ctrl-C` (`SIGINT`). Used to
/// drive axum's graceful shutdown.
async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("substrate_server: shutdown signal received, draining");
}
