//! Phase 5 — Tokio + axum webhook receiver.
//!
//! The pre-Phase-5 substrate had no in-process HTTP server: the
//! deployment pattern was to terminate webhook traffic at a separate
//! cloud relay (Cloudflare Workers, a small Node forwarder) which
//! then poked the substrate's IPC channel. That's still supported,
//! but several deployment modes — a fully self-hosted on-prem
//! substrate, a desktop host that exposes its own loopback receiver
//! for the synthesis pipeline's test fixtures, a per-tenant
//! container — want the substrate to terminate webhooks itself.
//!
//! This module ships a real, working webhook receiver on top of
//! `axum` 0.7 (`hyper` 1.x + `tower`):
//!
//! * binds a `tokio::net::TcpListener` on the configured address,
//! * registers a `POST /webhooks/{provider_id}` route that reads
//!   the request body, looks up the matching
//!   [`WebhookDispatch`] entry, and hands the body to the
//!   registered [`WebhookDispatcher`],
//! * registers a `GET /healthz` liveness endpoint that returns
//!   `200 OK` with the substrate's build version (callers can use
//!   this for load-balancer health checks),
//! * surfaces graceful shutdown via [`WebhookServer::run_until`]
//!   (oneshot-driven) and [`WebhookServer::serve_on`]
//!   (arbitrary-future-driven) so the substrate can drive the
//!   receiver from its own lifecycle without leaking sockets.
//!
//! The dispatcher trait is intentionally generic: it doesn't
//! mention `Connector` directly. That lets the substrate plumb the
//! receiver into whatever queue / fanout mechanism it uses (in the
//! current substrate, an `mpsc::Sender<(ConnectorInstanceId,
//! Vec<u8>)>` feeds the connector runtime; tests in this module
//! wire it to a `tokio::sync::Mutex<Vec<…>>` collector).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use axum::{
    body::Bytes,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Router,
};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

use crate::error::{ConnectorError, Result};

/// Async dispatcher invoked when the receiver accepts a webhook
/// POST.
///
/// Implementors are expected to:
///
/// 1. Look up the matching connector instance for `provider_id`.
/// 2. Run any provider-side signature verification (`X-Hub-Signature`,
///    `Stripe-Signature`, …) using the connector's stored secret —
///    the receiver does NOT verify signatures itself because each
///    provider uses a different scheme.
/// 3. Translate the body into substrate-side events.
///
/// Returning `Ok(())` causes the receiver to reply `200 OK`.
/// Returning `Err(ConnectorError::Webhook(_))` causes the receiver
/// to reply `400 Bad Request` (the body was malformed for the
/// declared provider). Any other `Err` variant becomes
/// `502 Bad Gateway` (the dispatcher itself failed — e.g. a
/// downstream queue is unreachable).
#[async_trait]
pub trait WebhookDispatcher: Send + Sync {
    /// Dispatch one webhook payload for the given provider.
    async fn dispatch(&self, provider_id: &str, body: Bytes) -> Result<()>;
}

/// Lookup row used by the receiver to route incoming `POST
/// /webhooks/{provider_id}` requests. Each row carries the
/// dispatcher to invoke for that provider id; in production the
/// substrate populates one row per registered connector instance
/// so connector-A's dispatcher never sees connector-B's payloads.
#[derive(Clone)]
pub struct WebhookDispatch {
    /// Provider id matched in the URL path.
    pub provider_id: String,
    /// Dispatcher invoked when this provider id is hit.
    pub dispatcher: Arc<dyn WebhookDispatcher>,
}

/// Static config for [`WebhookServer`]. The receiver does not
/// embed TLS termination — production deployments terminate TLS
/// at the ingress / sidecar layer. The substrate's loopback
/// receivers (used by the synthesis pipeline tests) speak plain
/// HTTP on `127.0.0.1`.
#[derive(Debug, Clone)]
pub struct WebhookServerConfig {
    /// Address to bind on. `0.0.0.0:port` for production; the test
    /// suite uses `127.0.0.1:0` to let the OS pick a free port.
    pub bind_addr: SocketAddr,
}

impl WebhookServerConfig {
    /// Build a config bound to the given address.
    #[must_use]
    pub fn new(bind_addr: SocketAddr) -> Self {
        Self { bind_addr }
    }
}

/// Shared state plumbed through axum's router. Tracks the
/// dispatcher table so the path-extractor handler can route the
/// incoming `provider_id` to the right dispatcher.
#[derive(Clone)]
struct ServerState {
    dispatchers: Arc<HashMap<String, Arc<dyn WebhookDispatcher>>>,
}

/// Real, working webhook receiver server.
///
/// Construct via [`WebhookServer::new`] with the list of
/// [`WebhookDispatch`] rows the substrate wants to expose. Call
/// [`WebhookServer::run_until`] (or [`WebhookServer::serve`]) to
/// start serving on the configured address.
pub struct WebhookServer {
    config: WebhookServerConfig,
    state: ServerState,
}

impl WebhookServer {
    /// Construct a server with the given config and dispatcher table.
    ///
    /// Duplicate `provider_id` entries in `dispatches` are merged —
    /// the last registration wins. This matches the substrate's
    /// per-connector lifecycle, where a re-subscribed connector
    /// rebinds the dispatcher even though the path is unchanged.
    #[must_use]
    pub fn new(config: WebhookServerConfig, dispatches: Vec<WebhookDispatch>) -> Self {
        let mut table: HashMap<String, Arc<dyn WebhookDispatcher>> =
            HashMap::with_capacity(dispatches.len());
        for d in dispatches {
            table.insert(d.provider_id, d.dispatcher);
        }
        Self {
            config,
            state: ServerState {
                dispatchers: Arc::new(table),
            },
        }
    }

    /// Borrow the active config. Useful in tests where the caller
    /// passed `127.0.0.1:0` and needs to discover the OS-picked
    /// port — but only *before* `serve` consumes the listener;
    /// after `serve` starts, query [`WebhookServer::bind`] for the
    /// live socket address.
    #[must_use]
    pub fn config(&self) -> &WebhookServerConfig {
        &self.config
    }

    /// Bind a TCP listener at the configured address without
    /// starting to serve. Lets tests grab the live socket address
    /// (when the config requested an ephemeral port via
    /// `127.0.0.1:0`) and only then pass the listener back via
    /// [`Self::serve_on`].
    ///
    /// # Errors
    ///
    /// Returns [`ConnectorError::Transport`] if the bind fails
    /// (port in use, permission denied, …).
    pub async fn bind(&self) -> Result<TcpListener> {
        TcpListener::bind(self.config.bind_addr)
            .await
            .map_err(|e| ConnectorError::Transport(format!("webhook bind failed: {e}")))
    }

    /// Serve forever (until the underlying tokio runtime is
    /// dropped). Most callers should prefer [`Self::run_until`]
    /// (oneshot-driven) or [`Self::serve_on`] (arbitrary-future
    /// driven) so they can drive shutdown from the substrate's
    /// lifecycle without leaking the listener socket on teardown.
    ///
    /// # Errors
    ///
    /// Returns [`ConnectorError::Transport`] if the axum server
    /// returns an unrecoverable error from `serve`.
    pub async fn serve(self) -> Result<()> {
        let listener = self.bind().await?;
        self.serve_on(listener, std::future::pending::<()>()).await
    }

    /// Serve on a pre-bound listener with a custom shutdown
    /// future. The future resolves => the receiver stops accepting
    /// new connections and waits for in-flight requests to finish.
    ///
    /// # Errors
    ///
    /// Returns [`ConnectorError::Transport`] if the axum server
    /// returns an unrecoverable error.
    pub async fn serve_on<F>(self, listener: TcpListener, shutdown: F) -> Result<()>
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let router = Router::new()
            .route("/healthz", get(healthz_handler))
            .route("/webhooks/:provider_id", post(webhook_handler))
            .with_state(self.state);

        axum::serve(listener, router)
            .with_graceful_shutdown(shutdown)
            .await
            .map_err(|e| ConnectorError::Transport(format!("webhook serve failed: {e}")))
    }

    /// Convenience: bind, serve until a shutdown signal arrives,
    /// then return. The shutdown channel is consumed; the caller
    /// holds the matching `Sender` and triggers shutdown by
    /// dropping it or calling `send(())`.
    ///
    /// # Errors
    ///
    /// Returns [`ConnectorError::Transport`] if bind or serve
    /// fails.
    pub async fn run_until(self, mut shutdown_rx: oneshot::Receiver<()>) -> Result<()> {
        let listener = self.bind().await?;
        self.serve_on(listener, async move {
            // Resolves on either Sender::send(()) or Sender drop.
            let _ = (&mut shutdown_rx).await;
        })
        .await
    }
}

/// Health-check route handler. Returns the substrate's build
/// version so external load balancers can pin a build during a
/// rolling deploy.
async fn healthz_handler() -> impl IntoResponse {
    (
        StatusCode::OK,
        [("content-type", "application/json")],
        format!(
            r#"{{"status":"ok","version":"{}"}}"#,
            env!("CARGO_PKG_VERSION")
        ),
    )
}

/// Webhook route handler. Looks up the dispatcher for
/// `provider_id`, hands it the body, and translates the result
/// into an HTTP response.
async fn webhook_handler(
    Path(provider_id): Path<String>,
    State(state): State<ServerState>,
    body: Bytes,
) -> impl IntoResponse {
    let Some(dispatcher) = state.dispatchers.get(&provider_id).cloned() else {
        return (StatusCode::NOT_FOUND, "unknown provider_id".to_string());
    };
    match dispatcher.dispatch(&provider_id, body).await {
        Ok(()) => (StatusCode::OK, "ok".into()),
        Err(ConnectorError::Webhook(msg)) => (StatusCode::BAD_REQUEST, msg),
        // Deliberately opaque: any non-`Webhook` `ConnectorError` is
        // a substrate-side failure (`Transport`, `Auth`,
        // `Permission`, an upstream queue, an internal serialization
        // bug, …). The external provider that POSTed this webhook
        // has no business seeing the substrate's internal error
        // strings — those can contain queue names, redacted-but-not
        // -fully-scrubbed URLs, auth-state hints, or upstream tenant
        // ids. Dispatchers are responsible for their own
        // server-side logging / tracing of the underlying `Err(e)`;
        // the receiver just signals "upstream is unhealthy, retry
        // later" via 502 with a fixed body. (Webhook providers
        // typically only inspect the status code anyway and retry
        // any 5xx.)
        Err(_) => (
            StatusCode::BAD_GATEWAY,
            "internal dispatcher error".to_string(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::Duration;
    use tokio::sync::oneshot;

    /// Collector dispatcher: records every (provider, body) pair
    /// it receives so the test can assert against it.
    #[derive(Default)]
    struct CollectorDispatcher {
        received: Mutex<Vec<(String, Vec<u8>)>>,
        mode: Mode,
    }

    #[derive(Default, Clone, Copy)]
    enum Mode {
        #[default]
        Ok,
        WebhookErr,
        OtherErr,
    }

    #[async_trait]
    impl WebhookDispatcher for CollectorDispatcher {
        async fn dispatch(&self, provider_id: &str, body: Bytes) -> Result<()> {
            self.received
                .lock()
                .expect("lock")
                .push((provider_id.to_string(), body.to_vec()));
            match self.mode {
                Mode::Ok => Ok(()),
                Mode::WebhookErr => Err(ConnectorError::Webhook("bad sig".into())),
                Mode::OtherErr => Err(ConnectorError::Transport("queue down".into())),
            }
        }
    }

    /// Spawn a server bound on `127.0.0.1:0`, return (live addr,
    /// shutdown sender, server JoinHandle, collector).
    async fn spawn_test_server(
        mode: Mode,
    ) -> (
        SocketAddr,
        oneshot::Sender<()>,
        tokio::task::JoinHandle<Result<()>>,
        Arc<CollectorDispatcher>,
    ) {
        let collector = Arc::new(CollectorDispatcher {
            received: Mutex::new(Vec::new()),
            mode,
        });
        let dispatches = vec![WebhookDispatch {
            provider_id: "slack".into(),
            dispatcher: collector.clone() as Arc<dyn WebhookDispatcher>,
        }];
        let cfg = WebhookServerConfig::new("127.0.0.1:0".parse().expect("addr"));
        let server = WebhookServer::new(cfg, dispatches);
        let listener = server.bind().await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let (tx, rx) = oneshot::channel::<()>();
        let handle = tokio::spawn(async move {
            server
                .serve_on(listener, async move {
                    let _ = rx.await;
                })
                .await
        });
        // Give axum a beat to wire the router on slower runners.
        tokio::time::sleep(Duration::from_millis(10)).await;
        (addr, tx, handle, collector)
    }

    #[tokio::test]
    async fn webhook_post_routes_to_registered_dispatcher() {
        let (addr, shutdown, handle, collector) = spawn_test_server(Mode::Ok).await;

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://{addr}/webhooks/slack"))
            .body("hello slack")
            .send()
            .await
            .expect("send");
        assert_eq!(resp.status(), 200);

        let collected = collector.received.lock().expect("lock").clone();
        assert_eq!(collected.len(), 1);
        assert_eq!(collected[0].0, "slack");
        assert_eq!(collected[0].1, b"hello slack");

        let _ = shutdown.send(());
        handle.await.expect("join").expect("serve");
    }

    #[tokio::test]
    async fn unknown_provider_returns_404() {
        let (addr, shutdown, handle, _collector) = spawn_test_server(Mode::Ok).await;

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://{addr}/webhooks/notion"))
            .body("hello notion")
            .send()
            .await
            .expect("send");
        assert_eq!(resp.status(), 404);

        let _ = shutdown.send(());
        handle.await.expect("join").expect("serve");
    }

    #[tokio::test]
    async fn dispatcher_webhook_error_maps_to_400() {
        let (addr, shutdown, handle, _collector) = spawn_test_server(Mode::WebhookErr).await;

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://{addr}/webhooks/slack"))
            .body("bad payload")
            .send()
            .await
            .expect("send");
        assert_eq!(resp.status(), 400);

        let _ = shutdown.send(());
        handle.await.expect("join").expect("serve");
    }

    #[tokio::test]
    async fn dispatcher_other_error_maps_to_502_with_opaque_body() {
        // CollectorDispatcher in `OtherErr` mode returns
        // `ConnectorError::Transport("queue down")`. The receiver
        // MUST translate that into a 502 with a generic body so the
        // external provider cannot learn anything about the
        // substrate's internal queue topology. In particular, the
        // body must NOT echo back the dispatcher's
        // `e.to_string()` (which would include "queue down").
        let (addr, shutdown, handle, _collector) = spawn_test_server(Mode::OtherErr).await;

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://{addr}/webhooks/slack"))
            .body("any")
            .send()
            .await
            .expect("send");
        assert_eq!(resp.status(), 502);
        let body = resp.text().await.expect("body");
        assert_eq!(body, "internal dispatcher error");
        assert!(
            !body.contains("queue down"),
            "502 body must not leak ConnectorError detail; got: {body}"
        );

        let _ = shutdown.send(());
        handle.await.expect("join").expect("serve");
    }

    #[tokio::test]
    async fn healthz_returns_200_with_version() {
        let (addr, shutdown, handle, _collector) = spawn_test_server(Mode::Ok).await;

        let client = reqwest::Client::new();
        let resp = client
            .get(format!("http://{addr}/healthz"))
            .send()
            .await
            .expect("send");
        assert_eq!(resp.status(), 200);
        let body = resp.text().await.expect("body");
        assert!(body.contains("\"status\":\"ok\""), "got: {body}");
        assert!(
            body.contains(env!("CARGO_PKG_VERSION")),
            "version not in body: {body}",
        );

        let _ = shutdown.send(());
        handle.await.expect("join").expect("serve");
    }

    #[tokio::test]
    async fn shutdown_signal_drops_listener() {
        let (addr, shutdown, handle, _collector) = spawn_test_server(Mode::Ok).await;

        // Trigger shutdown immediately. The `Result` from
        // `oneshot::Sender::send` is just "did anyone hear me?";
        // we don't care because the only listener is the server
        // task and a shutdown signal that arrived after it
        // already exited is a no-op success.
        let _ = shutdown.send(());
        // The server task should complete within a short window.
        // `Server::serve` returns `Result<(), io::Error>` wrapped
        // in `JoinError`; `expect` both layers to surface a clean
        // panic message if the server died unexpectedly.
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("server stopped within deadline")
            .expect("join")
            .expect("serve");

        // Connecting again should fail since the listener is gone.
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(200))
            .build()
            .expect("client");
        let err = client
            .get(format!("http://{addr}/healthz"))
            .send()
            .await
            .err();
        assert!(err.is_some(), "expected connection to fail after shutdown");
    }
}
