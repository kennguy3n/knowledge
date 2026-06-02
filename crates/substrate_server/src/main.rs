//! Substrate REST server — wraps every [`ffi`] function behind axum
//! endpoints on `127.0.0.1:9090` (internal loopback only).
//!
//! The Go API gateway on `:8080` proxies tenant-facing traffic here.
//! This binary is **not** exposed to the public internet.

use std::env;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use tracing_subscriber::EnvFilter;

use ffi::runtime::RuntimeHandle;
use ffi::types::{
    FfiImportanceClass, MemoryFilter, MemoryRecord, MemoryState, QueryResult, SourceKind,
    SynthesisTrigger,
};

// ---------------------------------------------------------------------------
// App state
// ---------------------------------------------------------------------------

/// Shared state handed to every handler via axum's `State` extractor.
struct AppState {
    handle: RuntimeHandle,
}

// ---------------------------------------------------------------------------
// Error mapping
// ---------------------------------------------------------------------------

/// Thin wrapper so we can implement `IntoResponse` for `ffi::FfiError`.
struct AppError(ffi::FfiError);

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = match &self.0 {
            ffi::FfiError::Unavailable { .. } => StatusCode::SERVICE_UNAVAILABLE,
            ffi::FfiError::NotFound { .. } => StatusCode::NOT_FOUND,
            ffi::FfiError::InvalidId { .. } => StatusCode::BAD_REQUEST,
            ffi::FfiError::Throttled { .. } => StatusCode::TOO_MANY_REQUESTS,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        let body = serde_json::json!({ "error": self.0.to_string() });
        (status, Json(body)).into_response()
    }
}

impl From<ffi::FfiError> for AppError {
    fn from(e: ffi::FfiError) -> Self {
        Self(e)
    }
}

type AppResult<T> = Result<Json<T>, AppError>;

// ---------------------------------------------------------------------------
// Request / response DTOs
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct IngestRequest {
    scope_id: String,
    body: String,
    source: SourceKind,
    importance: FfiImportanceClass,
}

#[derive(Serialize)]
struct IngestResponse {
    evidence_id: String,
}

#[derive(Deserialize)]
struct QueryRequest {
    scope_id: String,
    query_text: String,
    #[serde(default = "default_limit")]
    limit: u32,
}

fn default_limit() -> u32 {
    20
}

#[derive(Serialize)]
struct QueryResponse {
    results: Vec<QueryResult>,
}

#[derive(Deserialize)]
struct MemoriesQuery {
    scope_id: String,
    #[serde(default)]
    state: Option<MemoryState>,
    #[serde(default)]
    pinned_only: bool,
}

#[derive(Deserialize)]
struct SynthesisTriggerRequest {
    scope_id: String,
    #[serde(default = "default_trigger")]
    trigger: SynthesisTrigger,
}

fn default_trigger() -> SynthesisTrigger {
    SynthesisTrigger::ManualUserAction
}

#[derive(Serialize)]
struct SynthesisTriggerResponse {
    window_id: String,
}

#[derive(Deserialize)]
struct EncryptRequest {
    scope_id: String,
    /// Base64-encoded plaintext.
    plaintext_b64: String,
}

#[derive(Serialize)]
struct EncryptResponse {
    /// Base64-encoded `nonce || ciphertext`.
    ciphertext_b64: String,
}

#[derive(Deserialize)]
struct DecryptRequest {
    scope_id: String,
    /// Base64-encoded `nonce || ciphertext`.
    ciphertext_b64: String,
}

#[derive(Serialize)]
struct DecryptResponse {
    /// Base64-encoded plaintext.
    plaintext_b64: String,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn handle_ingest(
    State(state): State<Arc<AppState>>,
    Json(req): Json<IngestRequest>,
) -> AppResult<IngestResponse> {
    let id = ffi::ingest_message(
        state.handle,
        req.scope_id,
        req.body,
        req.source,
        req.importance,
    )?;
    Ok(Json(IngestResponse { evidence_id: id }))
}

async fn handle_query(
    State(state): State<Arc<AppState>>,
    Json(req): Json<QueryRequest>,
) -> AppResult<QueryResponse> {
    let results = ffi::query(state.handle, req.scope_id, req.query_text, req.limit)?;
    Ok(Json(QueryResponse { results }))
}

async fn handle_get_evidence(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> AppResult<ffi::types::EvidenceRecord> {
    let record = ffi::get_evidence(state.handle, id)?;
    Ok(Json(record))
}

async fn handle_forget(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    ffi::forget(state.handle, id)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn handle_forget_scope(
    State(state): State<Arc<AppState>>,
    Path(scope_id): Path<String>,
) -> Result<StatusCode, AppError> {
    ffi::forget_scope(state.handle, scope_id)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn handle_list_memories(
    State(state): State<Arc<AppState>>,
    Query(q): Query<MemoriesQuery>,
) -> AppResult<Vec<MemoryRecord>> {
    let filter = MemoryFilter {
        state: q.state,
        pinned_only: q.pinned_only,
    };
    let records = ffi::list_memories(state.handle, q.scope_id, filter)?;
    Ok(Json(records))
}

async fn handle_decay_sweep(
    State(state): State<Arc<AppState>>,
    Path(scope_id): Path<String>,
) -> AppResult<serde_json::Value> {
    let count = ffi::run_decay_sweep(state.handle, scope_id)?;
    Ok(Json(serde_json::json!({ "archived_count": count })))
}

async fn handle_get_channel_memory(
    State(state): State<Arc<AppState>>,
    Path(scope_id): Path<String>,
) -> AppResult<Option<MemoryRecord>> {
    let record = ffi::get_channel_memory(state.handle, scope_id)?;
    Ok(Json(record))
}

async fn handle_trigger_synthesis(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SynthesisTriggerRequest>,
) -> AppResult<SynthesisTriggerResponse> {
    let window_id = ffi::trigger_synthesis(state.handle, req.scope_id, req.trigger)?;
    Ok(Json(SynthesisTriggerResponse { window_id }))
}

async fn handle_generate_keypair() -> AppResult<ffi::types::FfiKeypair> {
    let kp = ffi::generate_keypair()?;
    Ok(Json(kp))
}

async fn handle_encrypt(
    State(state): State<Arc<AppState>>,
    Json(req): Json<EncryptRequest>,
) -> Result<Json<EncryptResponse>, AppError> {
    use base64::Engine;
    let plaintext = base64::engine::general_purpose::STANDARD
        .decode(&req.plaintext_b64)
        .map_err(|e| {
            AppError(ffi::FfiError::InvalidId {
                message: format!("invalid base64 plaintext: {e}"),
            })
        })?;
    let ct = ffi::encrypt(state.handle, req.scope_id, plaintext)?;
    let encoded = base64::engine::general_purpose::STANDARD.encode(&ct);
    Ok(Json(EncryptResponse {
        ciphertext_b64: encoded,
    }))
}

async fn handle_decrypt(
    State(state): State<Arc<AppState>>,
    Json(req): Json<DecryptRequest>,
) -> Result<Json<DecryptResponse>, AppError> {
    use base64::Engine;
    let ciphertext = base64::engine::general_purpose::STANDARD
        .decode(&req.ciphertext_b64)
        .map_err(|e| {
            AppError(ffi::FfiError::InvalidId {
                message: format!("invalid base64 ciphertext: {e}"),
            })
        })?;
    let pt = ffi::decrypt(state.handle, req.scope_id, ciphertext)?;
    let encoded = base64::engine::general_purpose::STANDARD.encode(&pt);
    Ok(Json(DecryptResponse {
        plaintext_b64: encoded,
    }))
}

/// Prometheus text exposition format for the metrics snapshot.
async fn handle_metrics() -> impl IntoResponse {
    let snap = ffi::metrics_snapshot();
    let json = serde_json::to_value(&snap).unwrap_or_default();
    let mut lines = Vec::new();
    if let serde_json::Value::Object(map) = json {
        for (key, value) in &map {
            if key == "errors_by_kind" {
                if let serde_json::Value::Object(inner) = value {
                    for (ek, ev) in inner {
                        let num = ev.as_u64().unwrap_or(0);
                        lines.push(format!(
                            "# TYPE knowledge_errors_{ek} counter\nknowledge_errors_{ek} {num}"
                        ));
                    }
                }
                continue;
            }
            if key == "retrieval_metrics" {
                if let serde_json::Value::Object(inner) = value {
                    for (rk, rv) in inner {
                        let num = rv.as_u64().unwrap_or(0);
                        lines.push(format!(
                            "# TYPE knowledge_retrieval_{rk} counter\nknowledge_retrieval_{rk} {num}"
                        ));
                    }
                }
                continue;
            }
            let num = value.as_u64().unwrap_or(0);
            let prom_type = if key.ends_with("_total") || key.ends_with("_count") {
                "counter"
            } else {
                "gauge"
            };
            lines.push(format!(
                "# TYPE knowledge_{key} {prom_type}\nknowledge_{key} {num}"
            ));
        }
    }
    (
        StatusCode::OK,
        [("content-type", "text/plain; version=0.0.4; charset=utf-8")],
        lines.join("\n"),
    )
}

/// Health check — probes all subsystems.
async fn handle_health(State(state): State<Arc<AppState>>) -> AppResult<ffi::HealthStatus> {
    let status = ffi::health_check(Some(state.handle))?;
    Ok(Json(status))
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    // Structured logging.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .json()
        .init();

    tracing::info!("substrate-server starting");

    // Open the evidence store with the master key from the environment.
    let master_key =
        env::var("KNOWLEDGE_MASTER_KEY").expect("KNOWLEDGE_MASTER_KEY env var must be set");

    let db_path = env::var("KNOWLEDGE_DB_PATH").unwrap_or_else(|_| "knowledge.db".into());

    let handle = ffi::open_store(master_key, db_path).expect("failed to open evidence store");

    tracing::info!(handle = handle.raw(), "evidence store opened");

    let state = Arc::new(AppState { handle });

    let app = Router::new()
        .route("/substrate/ingest", post(handle_ingest))
        .route("/substrate/query", post(handle_query))
        .route("/substrate/evidence/{id}", get(handle_get_evidence))
        .route("/substrate/forget/{id}", post(handle_forget))
        .route(
            "/substrate/forget-scope/{scope_id}",
            post(handle_forget_scope),
        )
        .route("/substrate/memories", get(handle_list_memories))
        .route(
            "/substrate/decay-sweep/{scope_id}",
            post(handle_decay_sweep),
        )
        .route(
            "/substrate/channel-memory/{scope_id}",
            get(handle_get_channel_memory),
        )
        .route(
            "/substrate/synthesis/trigger",
            post(handle_trigger_synthesis),
        )
        .route("/substrate/keypair", post(handle_generate_keypair))
        .route("/substrate/encrypt", post(handle_encrypt))
        .route("/substrate/decrypt", post(handle_decrypt))
        .route("/substrate/metrics", get(handle_metrics))
        .route("/substrate/health", get(handle_health))
        .with_state(state.clone());

    let bind_addr: SocketAddr = env::var("SUBSTRATE_BIND_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:9090".into())
        .parse()
        .expect("invalid SUBSTRATE_BIND_ADDR");

    tracing::info!(%bind_addr, "listening");

    let listener = tokio::net::TcpListener::bind(bind_addr)
        .await
        .expect("failed to bind");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("server error");

    // Tear down the evidence store on shutdown.
    tracing::info!("shutting down evidence store");
    if let Err(e) = ffi::close_store(handle) {
        tracing::error!(error = %e, "close_store failed during shutdown");
    }
    tracing::info!("substrate-server stopped");
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => tracing::info!("received SIGINT"),
        () = terminate => tracing::info!("received SIGTERM"),
    }
}
