//! The axum-based relay HTTP server.
//!
//! Routes (all blob bytes are opaque ciphertext to the server):
//!
//! * `POST /v1/topics/{topic}/deltas` — append sealed deltas to a
//!   topic; body [`PushRequest`], response [`PushResponse`].
//! * `GET  /v1/topics/{topic}/deltas/{since}` — read every blob with
//!   offset `> since`; response [`PullPage`].
//! * `GET  /healthz` — liveness probe.
//!
//! Every `/v1` request must carry `Authorization: Bearer <token>`;
//! the token resolves to a [`TenantId`] and topics are stored under
//! `(tenant, topic)` so tenants are isolated. The `since` watermark is
//! a path segment (not a query parameter) so the server needs no
//! `query` axum feature.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{header::AUTHORIZATION, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use tokio::net::TcpListener;

use sync_engine::transport::TopicId;

use crate::auth::{bearer_token, TenantId, TokenRegistry};
use crate::error::RelayError;
use crate::store::BlobStore;
use crate::wire::{PushRequest, PushResponse};

/// Shared, cheaply-cloneable server state plumbed through axum.
#[derive(Clone)]
pub struct RelayState {
    store: Arc<dyn BlobStore>,
    tokens: Arc<TokenRegistry>,
}

impl RelayState {
    /// Build relay state from a blob store and a token registry.
    pub fn new(store: Arc<dyn BlobStore>, tokens: Arc<TokenRegistry>) -> Self {
        Self { store, tokens }
    }
}

/// Static config for [`RelayServer`].
#[derive(Debug, Clone, Copy)]
pub struct RelayConfig {
    /// Address to bind on. Production uses `0.0.0.0:port` behind a
    /// TLS-terminating ingress; tests use `127.0.0.1:0` for an
    /// ephemeral port.
    pub bind_addr: SocketAddr,
}

impl RelayConfig {
    /// Build a config bound to `bind_addr`.
    pub fn new(bind_addr: SocketAddr) -> Self {
        Self { bind_addr }
    }
}

/// The relay HTTP server.
///
/// Lifecycle mirrors the substrate's other axum servers: [`bind`]
/// then [`serve_on`] (so tests can discover an ephemeral port before
/// serving), or [`run_until`] for the one-call "serve until shutdown
/// signal" path.
///
/// [`bind`]: Self::bind
/// [`serve_on`]: Self::serve_on
/// [`run_until`]: Self::run_until
pub struct RelayServer {
    config: RelayConfig,
    state: RelayState,
}

impl RelayServer {
    /// Construct a server from config and shared state.
    pub fn new(config: RelayConfig, state: RelayState) -> Self {
        Self { config, state }
    }

    /// Borrow the config (e.g. to read the configured bind address).
    pub fn config(&self) -> &RelayConfig {
        &self.config
    }

    /// Bind a TCP listener without serving, so callers can read the
    /// OS-assigned port (when binding `:0`) before handing the
    /// listener to [`Self::serve_on`].
    ///
    /// # Errors
    ///
    /// [`RelayError::Bind`] if the bind fails.
    pub async fn bind(&self) -> Result<TcpListener, RelayError> {
        TcpListener::bind(self.config.bind_addr)
            .await
            .map_err(|e| RelayError::Bind(e.to_string()))
    }

    /// Serve on a pre-bound listener until `shutdown` resolves.
    ///
    /// # Errors
    ///
    /// [`RelayError::Serve`] if axum returns an unrecoverable error.
    pub async fn serve_on<F>(self, listener: TcpListener, shutdown: F) -> Result<(), RelayError>
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let router = build_router(self.state);
        axum::serve(listener, router)
            .with_graceful_shutdown(shutdown)
            .await
            .map_err(|e| RelayError::Serve(e.to_string()))
    }

    /// Bind and serve until `shutdown_rx` fires (or its sender drops).
    ///
    /// # Errors
    ///
    /// [`RelayError::Bind`] / [`RelayError::Serve`] on failure.
    pub async fn run_until(
        self,
        mut shutdown_rx: tokio::sync::oneshot::Receiver<()>,
    ) -> Result<(), RelayError> {
        let listener = self.bind().await?;
        self.serve_on(listener, async move {
            let _ = (&mut shutdown_rx).await;
        })
        .await
    }
}

/// Build the relay router. Extracted so tests can drive it directly.
pub fn build_router(state: RelayState) -> Router {
    Router::new()
        .route("/v1/topics/{topic}/deltas", post(push_handler))
        .route("/v1/topics/{topic}/deltas/{since}", get(pull_handler))
        .route("/healthz", get(health_handler))
        .with_state(state)
}

async fn health_handler() -> impl IntoResponse {
    (
        StatusCode::OK,
        concat!("sync_relay ", env!("CARGO_PKG_VERSION")),
    )
}

async fn push_handler(
    State(state): State<RelayState>,
    Path(topic_hex): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(tenant) = authenticate(&state, &headers) else {
        return unauthorized();
    };
    let Ok(topic) = TopicId::from_hex(&topic_hex) else {
        return error(StatusCode::BAD_REQUEST, "invalid topic id");
    };
    // Parse only after auth so unauthenticated callers never reach the
    // JSON parser.
    let req: PushRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return error(
                StatusCode::BAD_REQUEST,
                &format!("invalid request body: {e}"),
            )
        }
    };

    match state.store.append(&tenant, &topic, &req.blobs) {
        Ok(cursor) => {
            tracing::debug!(
                tenant = %tenant,
                topic = %topic_prefix(&topic_hex),
                blobs = req.blobs.len(),
                cursor,
                "relay push"
            );
            (StatusCode::OK, Json(PushResponse { cursor })).into_response()
        }
        Err(e @ RelayError::BlobTooLarge { .. }) => {
            error(StatusCode::PAYLOAD_TOO_LARGE, &e.to_string())
        }
        Err(e @ RelayError::QuotaExceeded { .. }) => {
            error(StatusCode::INSUFFICIENT_STORAGE, &e.to_string())
        }
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

async fn pull_handler(
    State(state): State<RelayState>,
    Path((topic_hex, since)): Path<(String, u64)>,
    headers: HeaderMap,
) -> Response {
    let Some(tenant) = authenticate(&state, &headers) else {
        return unauthorized();
    };
    let Ok(topic) = TopicId::from_hex(&topic_hex) else {
        return error(StatusCode::BAD_REQUEST, "invalid topic id");
    };

    match state.store.read_since(&tenant, &topic, since) {
        Ok(page) => {
            tracing::debug!(
                tenant = %tenant,
                topic = %topic_prefix(&topic_hex),
                since,
                returned = page.blobs.len(),
                next_cursor = page.next_cursor,
                "relay pull"
            );
            (StatusCode::OK, Json(page)).into_response()
        }
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// Resolve the request's bearer token to a tenant, or `None` if the
/// token is missing/invalid. The caller turns `None` into a `401`.
fn authenticate(state: &RelayState, headers: &HeaderMap) -> Option<TenantId> {
    let presented = headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(bearer_token);
    presented
        .and_then(|t| state.tokens.authenticate(t))
        .cloned()
}

fn unauthorized() -> Response {
    error(StatusCode::UNAUTHORIZED, "invalid or missing bearer token")
}

fn error(status: StatusCode, message: &str) -> Response {
    (status, message.to_owned()).into_response()
}

/// A short, log-safe prefix of a topic hex (a topic is a read
/// capability — never log it in full).
fn topic_prefix(topic_hex: &str) -> &str {
    &topic_hex[..8.min(topic_hex.len())]
}
