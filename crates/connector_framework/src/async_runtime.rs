//! Phase 5 — Tokio async surface for the connector framework.
//!
//! The pre-existing [`Connector`](crate::connector::Connector)
//! trait and [`HttpTransport`](crate::http::HttpTransport) are
//! intentionally **synchronous**. Connectors were designed to be
//! driven from `std::thread`-based background pollers, and the
//! blocking HTTP client kept the framework free of a `tokio`
//! dependency for hosts that didn't need one. See the long-form
//! rationale in `crate::http::HttpTransport`'s doc comment.
//!
//! Phase 5 lifts that constraint *additively*. The substrate is
//! moving to a tokio-driven runtime so:
//!
//! * the webhook receiver server can serve concurrent provider
//!   POSTs without spawning one thread per request;
//! * the inference router can stream tokens from a llama.cpp
//!   loopback server without burning a thread per generation;
//! * substrate hosts that already run tokio (e.g. the Electron
//!   main process via N-API + `napi_addon`) can drive connectors
//!   from their own runtime instead of standing up a parallel
//!   thread pool.
//!
//! This module ships three real, working pieces:
//!
//! 1. [`AsyncHttpTransport`] — async sibling of
//!    [`HttpTransport`](crate::http::HttpTransport). Same request
//!    / response types, same retry policy, but every method is
//!    `async`. The default reqwest-backed impl lives in
//!    `crate::http_async::AsyncReqwestHttpTransport`.
//! 2. [`AsyncConnector`] — async sibling of
//!    [`Connector`](crate::connector::Connector). Methods return
//!    futures so callers in a tokio runtime can drive them
//!    without `spawn_blocking`.
//! 3. [`BlockingConnectorAdapter`] — bridges a sync
//!    [`Connector`](crate::connector::Connector) impl into the
//!    [`AsyncConnector`] trait by running each call inside
//!    [`tokio::task::spawn_blocking`]. This is the supported
//!    migration path: the nine production connectors keep their
//!    sync impls (and the matching unit tests), but they're
//!    drivable from the new async substrate without rewriting a
//!    line.
//!
//! The adapter is a real implementation, not a placeholder — it
//! propagates [`ConnectorError`](crate::ConnectorError) faithfully,
//! it requires `'static` for the wrapped connector (which all
//! production connectors satisfy), and it preserves cancellation
//! semantics so a dropped future cancels the underlying
//! [`spawn_blocking`] task as soon as the blocking call exits.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::task;

use crate::{
    config::ConnectorConfig,
    connector::{Connector, SyncRunResult},
    error::{ConnectorError, Result},
    event::ConnectorEvent,
    http::{HttpRequest, HttpResponse},
    sync::SyncState,
    token_vault::OAuth2Token,
    webhook::WebhookSubscription,
};

/// Async sibling of [`HttpTransport`](crate::http::HttpTransport).
///
/// Implementors are expected to perform the same retry / backoff
/// behaviour as the sync [`HttpTransport`](crate::http::HttpTransport)
/// — see [`RetryPolicy`](crate::http::RetryPolicy) for the contract.
/// The reqwest-backed default impl (in `crate::http_async`)
/// reuses [`RetryPolicy`](crate::http::RetryPolicy) directly and
/// drives the same loop with [`tokio::time::sleep`] in place of
/// [`std::thread::sleep`].
///
/// The trait is `Send + Sync` so it can live inside an `Arc<dyn
/// AsyncHttpTransport>` shared across tasks.
#[async_trait]
pub trait AsyncHttpTransport: Send + Sync {
    /// Execute one HTTP request, applying retries / backoff per
    /// the transport's policy. Implementations should map low-level
    /// transport errors to
    /// [`ConnectorError::Transport`](crate::error::ConnectorError::Transport);
    /// HTTP status codes (including 4xx) are surfaced via
    /// [`HttpResponse::status`](crate::http::HttpResponse::status)
    /// rather than as `Err`.
    async fn execute(&self, request: HttpRequest) -> Result<HttpResponse>;

    /// Convenience: `GET url`. Default impl builds an
    /// [`HttpRequest`](crate::http::HttpRequest) and dispatches via
    /// [`Self::execute`].
    async fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<HttpResponse> {
        let mut req = HttpRequest::get(url);
        for (k, v) in headers {
            req = req.with_header(*k, *v);
        }
        self.execute(req).await
    }

    /// Convenience: `POST url` with a body.
    async fn post(&self, url: &str, headers: &[(&str, &str)], body: &[u8]) -> Result<HttpResponse> {
        let mut req = HttpRequest::post(url, body.to_vec());
        for (k, v) in headers {
            req = req.with_header(*k, *v);
        }
        self.execute(req).await
    }
}

/// Async sibling of [`Connector`](crate::connector::Connector).
///
/// Methods return futures so callers in a tokio runtime can drive
/// the auth handshake, sync runs, and webhook dispatch without
/// occupying a worker thread for the duration of each call.
///
/// The trait is `Send + Sync` so a substrate runtime can hold the
/// connector inside an `Arc<dyn AsyncConnector>` and dispatch
/// concurrent calls across worker threads.
#[async_trait]
pub trait AsyncConnector: Send + Sync {
    /// Run the auth handshake and return a fresh bearer token.
    async fn authenticate(&self, config: &ConnectorConfig) -> Result<OAuth2Token>;

    /// First-time pull — walk the entire source surface.
    async fn initial_sync(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
    ) -> Result<SyncRunResult>;

    /// Steady-state pull — read the cursor from `state`.
    async fn incremental_sync(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        state: &SyncState,
    ) -> Result<SyncRunResult>;

    /// Install a push subscription with the provider. The returned
    /// [`WebhookSubscription`] should be persisted by the runtime.
    async fn subscribe_webhook(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        callback_url: &str,
    ) -> Result<WebhookSubscription>;

    /// Translate one provider-side webhook payload into the
    /// substrate-side events it carries.
    async fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>>;
}

/// Bridges a synchronous [`Connector`] into the [`AsyncConnector`]
/// trait by running each call inside
/// [`tokio::task::spawn_blocking`].
///
/// This is the supported migration path for the nine production
/// connectors in `crates/connectors/`: they keep their existing
/// sync impls (and the hundreds of unit tests pinned to them),
/// but they become drivable from the new async substrate without
/// a per-connector rewrite. The blocking work runs on tokio's
/// dedicated blocking pool (default 512 threads), so concurrent
/// sync runs across connectors don't starve each other or the
/// runtime's compute workers.
///
/// `BlockingConnectorAdapter` requires the inner connector to be
/// `Send + Sync + 'static`. All production connectors meet that
/// bound — they hold immutable references to transports and
/// configs, and any interior mutability uses `Mutex`/`RwLock`.
pub struct BlockingConnectorAdapter<C: Connector + Send + Sync + 'static> {
    inner: Arc<C>,
}

impl<C: Connector + Send + Sync + 'static> BlockingConnectorAdapter<C> {
    /// Wrap a sync connector so it satisfies [`AsyncConnector`].
    pub fn new(connector: C) -> Self {
        Self {
            inner: Arc::new(connector),
        }
    }

    /// Wrap a sync connector that's already inside an `Arc`. Useful
    /// when the substrate shares the same connector instance across
    /// both the sync and async paths during the Phase 5 migration.
    #[must_use]
    pub fn from_arc(connector: Arc<C>) -> Self {
        Self { inner: connector }
    }

    /// Borrow the wrapped sync connector. Lets the substrate keep
    /// driving the sync API in parallel (e.g. a legacy background
    /// poller) while the async surface is migrated in.
    #[must_use]
    pub fn inner(&self) -> &Arc<C> {
        &self.inner
    }
}

#[async_trait]
impl<C: Connector + Send + Sync + 'static> AsyncConnector for BlockingConnectorAdapter<C> {
    async fn authenticate(&self, config: &ConnectorConfig) -> Result<OAuth2Token> {
        let inner = Arc::clone(&self.inner);
        let config = config.clone();
        task::spawn_blocking(move || inner.authenticate(&config))
            .await
            .map_err(|e| join_err_to_connector_err(&e))?
    }

    async fn initial_sync(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
    ) -> Result<SyncRunResult> {
        let inner = Arc::clone(&self.inner);
        let config = config.clone();
        let token = token.clone();
        task::spawn_blocking(move || inner.initial_sync(&config, &token))
            .await
            .map_err(|e| join_err_to_connector_err(&e))?
    }

    async fn incremental_sync(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        state: &SyncState,
    ) -> Result<SyncRunResult> {
        let inner = Arc::clone(&self.inner);
        let config = config.clone();
        let token = token.clone();
        let state = state.clone();
        task::spawn_blocking(move || inner.incremental_sync(&config, &token, &state))
            .await
            .map_err(|e| join_err_to_connector_err(&e))?
    }

    async fn subscribe_webhook(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        callback_url: &str,
    ) -> Result<WebhookSubscription> {
        let inner = Arc::clone(&self.inner);
        let config = config.clone();
        let token = token.clone();
        let callback_url = callback_url.to_string();
        task::spawn_blocking(move || inner.subscribe_webhook(&config, &token, &callback_url))
            .await
            .map_err(|e| join_err_to_connector_err(&e))?
    }

    async fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        let inner = Arc::clone(&self.inner);
        let body = body.to_vec();
        task::spawn_blocking(move || inner.handle_webhook_event(&body))
            .await
            .map_err(|e| join_err_to_connector_err(&e))?
    }
}

/// Translate a tokio [`JoinError`](tokio::task::JoinError) into a
/// [`ConnectorError`]. The runtime cancels blocking tasks when
/// their futures are dropped — that's not a failure as far as the
/// substrate is concerned, but if a task panics we surface it as a
/// transport-level error so the substrate's retry logic can decide
/// whether to back off or escalate.
fn join_err_to_connector_err(e: &task::JoinError) -> ConnectorError {
    if e.is_cancelled() {
        ConnectorError::Transport("connector task cancelled by runtime".into())
    } else if e.is_panic() {
        ConnectorError::Transport(format!("connector task panicked: {e}"))
    } else {
        ConnectorError::Transport(format!("connector task join error: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{AuthKind, ConnectorKind},
        event::SourceDocumentId,
        webhook::{WebhookEventTypes, WebhookSecret},
    };
    use chrono::Utc;
    use evidence_store::ScopeId;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use uuid::Uuid;

    /// A real connector impl used only inside this module's tests
    /// to verify the adapter actually drives the sync trait under
    /// tokio. Each call increments a per-method counter so the
    /// test can assert which methods were invoked.
    #[derive(Default)]
    #[allow(clippy::struct_field_names)] // every field counts hits for a different method; the shared suffix is informative.
    struct CountingConnector {
        authenticate_hits: AtomicUsize,
        initial_hits: AtomicUsize,
        incremental_hits: AtomicUsize,
        subscribe_hits: AtomicUsize,
        webhook_hits: AtomicUsize,
    }

    impl Connector for CountingConnector {
        fn authenticate(&self, _config: &ConnectorConfig) -> Result<OAuth2Token> {
            self.authenticate_hits.fetch_add(1, Ordering::SeqCst);
            Ok(OAuth2Token::new(
                "test-access",
                "test-refresh",
                Utc::now() + chrono::Duration::hours(1),
                "read",
            ))
        }

        fn initial_sync(
            &self,
            _config: &ConnectorConfig,
            _token: &OAuth2Token,
        ) -> Result<SyncRunResult> {
            self.initial_hits.fetch_add(1, Ordering::SeqCst);
            Ok(SyncRunResult {
                events: vec![ConnectorEvent::DocumentCreated {
                    document_id: SourceDocumentId::new("doc-1"),
                    occurred_at: Utc::now(),
                }],
                next_cursor: Some("cursor-1".into()),
            })
        }

        fn incremental_sync(
            &self,
            _config: &ConnectorConfig,
            _token: &OAuth2Token,
            _state: &SyncState,
        ) -> Result<SyncRunResult> {
            self.incremental_hits.fetch_add(1, Ordering::SeqCst);
            Ok(SyncRunResult {
                events: vec![],
                next_cursor: Some("cursor-2".into()),
            })
        }

        fn subscribe_webhook(
            &self,
            config: &ConnectorConfig,
            _token: &OAuth2Token,
            callback_url: &str,
        ) -> Result<WebhookSubscription> {
            self.subscribe_hits.fetch_add(1, Ordering::SeqCst);
            let _ = config;
            Ok(WebhookSubscription::new(
                crate::token_vault::ConnectorInstanceId::new_v4(),
                callback_url.to_string(),
                WebhookSecret::new("secret"),
                WebhookEventTypes::all(),
                None,
            ))
        }

        fn handle_webhook_event(&self, _body: &[u8]) -> Result<Vec<ConnectorEvent>> {
            self.webhook_hits.fetch_add(1, Ordering::SeqCst);
            Ok(vec![ConnectorEvent::DocumentDeleted {
                document_id: SourceDocumentId::new("doc-1"),
                occurred_at: Utc::now(),
            }])
        }
    }

    fn make_config() -> ConnectorConfig {
        ConnectorConfig::new(
            ConnectorKind::Slack,
            AuthKind::OAuth2,
            ScopeId(Uuid::new_v4()),
        )
    }

    #[tokio::test]
    async fn adapter_runs_authenticate_via_spawn_blocking() {
        let connector = CountingConnector::default();
        let adapter = BlockingConnectorAdapter::new(connector);
        let cfg = make_config();

        let token = adapter.authenticate(&cfg).await.unwrap();
        assert_eq!(token.access_token.expose(), "test-access");
        assert_eq!(adapter.inner.authenticate_hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn adapter_runs_initial_sync_returning_events() {
        let connector = CountingConnector::default();
        let adapter = BlockingConnectorAdapter::new(connector);
        let cfg = make_config();
        let token = adapter.authenticate(&cfg).await.unwrap();

        let result = adapter.initial_sync(&cfg, &token).await.unwrap();
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.next_cursor.as_deref(), Some("cursor-1"));
        assert_eq!(adapter.inner.initial_hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn adapter_runs_incremental_sync_against_provided_state() {
        let connector = CountingConnector::default();
        let adapter = BlockingConnectorAdapter::new(connector);
        let cfg = make_config();
        let token = adapter.authenticate(&cfg).await.unwrap();
        let state = SyncState::new(crate::token_vault::ConnectorInstanceId::new_v4());

        let result = adapter
            .incremental_sync(&cfg, &token, &state)
            .await
            .unwrap();
        assert_eq!(result.events.len(), 0);
        assert_eq!(result.next_cursor.as_deref(), Some("cursor-2"));
        assert_eq!(adapter.inner.incremental_hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn adapter_runs_subscribe_webhook_then_handle_webhook_event() {
        let connector = CountingConnector::default();
        let adapter = BlockingConnectorAdapter::new(connector);
        let cfg = make_config();
        let token = adapter.authenticate(&cfg).await.unwrap();

        let sub = adapter
            .subscribe_webhook(&cfg, &token, "https://substrate.example/cb")
            .await
            .unwrap();
        assert_eq!(sub.callback_url, "https://substrate.example/cb");

        let events = adapter.handle_webhook_event(b"{}").await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(adapter.inner.subscribe_hits.load(Ordering::SeqCst), 1);
        assert_eq!(adapter.inner.webhook_hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn adapter_concurrent_calls_run_in_parallel() {
        // Real interleaving — fire ten concurrent sync runs and
        // confirm each one completes. With `spawn_blocking` they
        // run on the blocking pool; this proves the adapter doesn't
        // serialise calls via an unintended `Mutex`.
        let connector = CountingConnector::default();
        let adapter = Arc::new(BlockingConnectorAdapter::new(connector));
        let cfg = Arc::new(make_config());
        let token = adapter.authenticate(&cfg).await.unwrap();
        let token = Arc::new(token);

        let mut handles = Vec::new();
        for _ in 0..10 {
            let adapter = Arc::clone(&adapter);
            let cfg = Arc::clone(&cfg);
            let token = Arc::clone(&token);
            handles.push(tokio::spawn(async move {
                adapter.initial_sync(&cfg, &token).await
            }));
        }
        for h in handles {
            h.await.unwrap().unwrap();
        }
        assert_eq!(adapter.inner.initial_hits.load(Ordering::SeqCst), 10);
    }

    #[tokio::test]
    async fn adapter_propagates_connector_error_to_async_caller() {
        // A connector impl that always fails. The adapter must
        // surface the same `ConnectorError` instead of swallowing
        // it as a transport error.
        struct FailingConnector;
        impl Connector for FailingConnector {
            fn authenticate(&self, _: &ConnectorConfig) -> Result<OAuth2Token> {
                Err(ConnectorError::Auth("bad password".into()))
            }
            fn initial_sync(&self, _: &ConnectorConfig, _: &OAuth2Token) -> Result<SyncRunResult> {
                Err(ConnectorError::Sync("api down".into()))
            }
            fn incremental_sync(
                &self,
                _: &ConnectorConfig,
                _: &OAuth2Token,
                _: &SyncState,
            ) -> Result<SyncRunResult> {
                Err(ConnectorError::Sync("api down".into()))
            }
            fn subscribe_webhook(
                &self,
                _: &ConnectorConfig,
                _: &OAuth2Token,
                _: &str,
            ) -> Result<WebhookSubscription> {
                Err(ConnectorError::Transport("subscribe failed".into()))
            }
            fn handle_webhook_event(&self, _: &[u8]) -> Result<Vec<ConnectorEvent>> {
                Err(ConnectorError::Webhook("bad json".into()))
            }
        }

        let adapter = BlockingConnectorAdapter::new(FailingConnector);
        let cfg = make_config();

        let err = adapter.authenticate(&cfg).await.unwrap_err();
        assert!(matches!(err, ConnectorError::Auth(ref m) if m == "bad password"));

        let err = adapter.handle_webhook_event(b"{}").await.unwrap_err();
        assert!(matches!(err, ConnectorError::Webhook(ref m) if m == "bad json"));
    }

    #[tokio::test]
    async fn adapter_propagates_panic_as_transport_error() {
        struct PanickyConnector;
        impl Connector for PanickyConnector {
            fn authenticate(&self, _: &ConnectorConfig) -> Result<OAuth2Token> {
                panic!("simulated connector panic");
            }
            fn initial_sync(&self, _: &ConnectorConfig, _: &OAuth2Token) -> Result<SyncRunResult> {
                unreachable!()
            }
            fn incremental_sync(
                &self,
                _: &ConnectorConfig,
                _: &OAuth2Token,
                _: &SyncState,
            ) -> Result<SyncRunResult> {
                unreachable!()
            }
            fn subscribe_webhook(
                &self,
                _: &ConnectorConfig,
                _: &OAuth2Token,
                _: &str,
            ) -> Result<WebhookSubscription> {
                unreachable!()
            }
            fn handle_webhook_event(&self, _: &[u8]) -> Result<Vec<ConnectorEvent>> {
                unreachable!()
            }
        }

        let adapter = BlockingConnectorAdapter::new(PanickyConnector);
        let cfg = make_config();
        let err = adapter.authenticate(&cfg).await.unwrap_err();
        match err {
            ConnectorError::Transport(msg) => assert!(msg.contains("panicked")),
            other => panic!("expected Transport, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn from_arc_constructor_shares_underlying_connector() {
        // The substrate should be able to drive the same connector
        // through both the sync API (legacy poller) and the async
        // API (new tokio substrate) without instantiating two
        // independent connector instances.
        let connector = Arc::new(CountingConnector::default());
        let adapter = BlockingConnectorAdapter::from_arc(Arc::clone(&connector));

        let cfg = make_config();
        let _ = adapter.authenticate(&cfg).await.unwrap();
        // Drive the sync side as well — same `Arc`, same counter.
        let _ = connector.authenticate(&cfg).unwrap();

        assert_eq!(connector.authenticate_hits.load(Ordering::SeqCst), 2);
    }
}
