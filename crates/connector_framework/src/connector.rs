//! The [`Connector`] trait and supporting sync result types.
//!
//! Per `docs/DESIGN.md` §10.2 a connector is the boundary between the
//! substrate and one external source system. The trait is kept
//! deliberately small and synchronous so it can be unit-tested
//! against in-memory fakes and so the same implementor compiles
//! on hosts without a tokio toolchain.
//!
//! Production hosts that want async dispatch wrap the sync
//! implementor in [`crate::async_runtime::BlockingConnectorAdapter`]
//! and run sync calls on tokio's blocking-task pool via
//! `spawn_blocking`, which exposes the async surface
//! [`crate::async_runtime::AsyncConnector`] without forcing every
//! connector to maintain two parallel impls.

use crate::config::ConnectorConfig;
use crate::error::{ConnectorError, Result};
use crate::event::{ConnectorEvent, FetchedContent, SourceDocumentId};
use crate::sync::SyncState;
use crate::token_vault::OAuth2Token;
use crate::webhook::WebhookSubscription;

/// Result of an `initial_sync` / `incremental_sync` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncRunResult {
    /// Events emitted by the run, in source order.
    pub events: Vec<ConnectorEvent>,
    /// New cursor to persist into [`SyncState::cursor`]. `None`
    /// means "no further pages".
    pub next_cursor: Option<String>,
}

/// A connector — the substrate's boundary against one source system.
///
/// Implementors are expected to:
///
/// * `authenticate` — perform the auth handshake (OAuth2 code
///   exchange, API key validation, …) and return the bearer token
///   to store in the [`crate::token_vault::OAuth2TokenVault`].
/// * `initial_sync` — full pull when the connector first comes up.
/// * `incremental_sync` — steady-state pull keyed off the
///   [`SyncState`] cursor.
/// * `fetch_content` — pull the materialised body of one document
///   (called lazily by the runtime for each `DocumentCreated` /
///   `DocumentUpdated` event the sync passes surface).
/// * `subscribe_webhook` — install a push subscription with the
///   provider so the substrate can react to changes without
///   polling.
/// * `handle_webhook_event` — translate one provider-side webhook
///   payload into a substrate-side [`ConnectorEvent`].
///
/// Ships only the trait + framework; the individual connectors
/// live in their own crates.
///
/// The trait is `Send + Sync` so a substrate runtime can keep one
/// or more connector instances inside an
/// `Arc<Mutex<…>>` / `Box<dyn Connector + Send + Sync>` and dispatch
/// calls across worker threads. This mirrors the
/// [`crate::http::HttpTransport`] supertrait bound — every concrete
/// connector in this workspace is naturally `Send + Sync` (their
/// fields are an `Arc<dyn HttpTransport>`, an `Arc<dyn OAuth2CodeExchange>`,
/// and `Copy` ids), so the supertrait is observation rather than a
/// new constraint on implementors.
pub trait Connector: Send + Sync {
    /// Run the auth handshake and return a fresh bearer token.
    fn authenticate(&self, config: &ConnectorConfig) -> Result<OAuth2Token>;

    /// First-time pull — walk the entire source surface.
    fn initial_sync(&self, config: &ConnectorConfig, token: &OAuth2Token) -> Result<SyncRunResult>;

    /// Steady-state pull — read the cursor from `state`.
    fn incremental_sync(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        state: &SyncState,
    ) -> Result<SyncRunResult>;

    /// Fetch the materialised body of a single source document.
    ///
    /// `initial_sync` / `incremental_sync` emit lightweight
    /// `(document, action, permission)` deltas — they carry only the
    /// [`SourceDocumentId`], never the document bytes, so the sync
    /// loop can move large change feeds cheaply. This method is the
    /// second half of the contract: given one document id, it issues
    /// the provider-specific content API call(s) over the injected
    /// [`crate::http::HttpTransport`] and returns the normalised
    /// [`FetchedContent`] (body bytes + MIME type + title + metadata
    /// + canonical URL).
    ///
    /// # Runtime wiring contract
    ///
    /// `fetch_content` is **not** called from the connector itself —
    /// the substrate runtime (`crates/substrate_server`) owns the
    /// orchestration. For every [`ConnectorEvent::DocumentCreated`]
    /// and [`ConnectorEvent::DocumentUpdated`] a sync run surfaces, the
    /// runtime is expected to:
    ///
    /// 1. call `fetch_content(config, token, event.document_id())` to
    ///    pull the body;
    /// 2. chunk large bodies with
    ///    `observation_engine::DocumentChunker` before ingest (so a
    ///    multi-megabyte page is split into embedding-sized windows
    ///    rather than ingested as one oversized record); and
    /// 3. feed each chunk into the substrate via `ffi::ingest_message`,
    ///    tagging it with the connector's scope and the document's
    ///    [`FetchedContent::source_url`] / [`FetchedContent::title`]
    ///    for citation.
    ///
    /// [`ConnectorEvent::DocumentDeleted`] events skip `fetch_content`
    /// entirely (there is nothing to fetch — the runtime issues a
    /// `forget` instead), and [`ConnectorEvent::PermissionChanged`]
    /// events route to the ACL projection rather than ingestion.
    ///
    /// The connector crate intentionally does **not** depend on
    /// `observation_engine` or `ffi` for this — chunking and ingest
    /// live on the runtime side of the trait boundary so connectors
    /// stay a thin, independently-testable HTTP layer.
    ///
    /// # Default implementation
    ///
    /// The default returns [`ConnectorError::Unimplemented`] so a
    /// connector that has not yet wired content fetching still
    /// compiles. Every production connector in this workspace
    /// overrides it; a runtime that receives `Unimplemented` should
    /// treat it as a wiring bug, not a transient sync error.
    ///
    /// # Errors
    ///
    /// Returns [`ConnectorError::Auth`] when the provider rejects the
    /// token (401/403), [`ConnectorError::Sync`] for any other non-2xx
    /// status, a malformed body, or an unfetchable document (e.g. a
    /// Google Workspace doc with no export path), and
    /// [`ConnectorError::Transport`] for low-level network failures.
    fn fetch_content(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        document_id: &SourceDocumentId,
    ) -> Result<FetchedContent> {
        let _ = (config, token);
        Err(ConnectorError::Unimplemented(format!(
            "fetch_content not implemented for document {document_id}"
        )))
    }

    /// Install a push subscription with the provider. The returned
    /// [`WebhookSubscription`] should be persisted by the runtime.
    fn subscribe_webhook(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        callback_url: &str,
    ) -> Result<WebhookSubscription>;

    /// Translate one provider-side webhook payload into the
    /// substrate-side events it carries.
    ///
    /// Most providers (HubSpot, Microsoft Graph, Confluence, …)
    /// deliver **batched** payloads — a single HTTP POST may carry
    /// several independent change notifications. Implementors must
    /// emit every event present in `body`; returning only the first
    /// one silently drops the rest. Single-event providers should
    /// return a one-element [`Vec`].
    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AuthKind, ConnectorKind};
    use crate::event::SourceDocumentId;
    use crate::token_vault::ConnectorInstanceId;
    use crate::webhook::{
        parse_webhook_event, WebhookEventTypes, WebhookSecret, WebhookSubscription,
    };
    use chrono::{Duration, Utc};
    use evidence_store::ScopeId;

    /// Fake connector used to exercise the trait surface in tests.
    struct FakeConnector {
        instance: ConnectorInstanceId,
    }

    impl Connector for FakeConnector {
        fn authenticate(&self, _config: &ConnectorConfig) -> Result<OAuth2Token> {
            Ok(OAuth2Token::new(
                "access",
                "refresh",
                Utc::now() + Duration::hours(1),
                "scope.read",
            ))
        }

        fn initial_sync(
            &self,
            _config: &ConnectorConfig,
            _token: &OAuth2Token,
        ) -> Result<SyncRunResult> {
            Ok(SyncRunResult {
                events: vec![ConnectorEvent::DocumentCreated {
                    document_id: SourceDocumentId::new("doc-1"),
                    occurred_at: Utc::now(),
                }],
                next_cursor: Some("page-2".into()),
            })
        }

        fn incremental_sync(
            &self,
            _config: &ConnectorConfig,
            _token: &OAuth2Token,
            _state: &SyncState,
        ) -> Result<SyncRunResult> {
            Ok(SyncRunResult {
                events: vec![],
                next_cursor: None,
            })
        }

        fn subscribe_webhook(
            &self,
            _config: &ConnectorConfig,
            _token: &OAuth2Token,
            callback_url: &str,
        ) -> Result<WebhookSubscription> {
            Ok(WebhookSubscription::new(
                self.instance,
                callback_url,
                WebhookSecret::new("fake-secret"),
                WebhookEventTypes::all(),
                None,
            ))
        }

        fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
            Ok(vec![parse_webhook_event(body)?])
        }
    }

    #[test]
    fn trait_surface_is_callable() {
        let inst = ConnectorInstanceId::new_v4();
        let connector = FakeConnector { instance: inst };
        let cfg = ConnectorConfig::new(ConnectorKind::Notion, AuthKind::OAuth2, ScopeId::new_v4());
        let tok = connector.authenticate(&cfg).unwrap();
        let sync = connector.initial_sync(&cfg, &tok).unwrap();
        assert_eq!(sync.events.len(), 1);
        assert_eq!(sync.next_cursor.as_deref(), Some("page-2"));
        let st = SyncState::new(inst);
        let inc = connector.incremental_sync(&cfg, &tok, &st).unwrap();
        assert!(inc.events.is_empty());
        let sub = connector
            .subscribe_webhook(&cfg, &tok, "https://substrate.example/webhook")
            .unwrap();
        assert_eq!(sub.connector, inst);
    }

    #[test]
    fn default_fetch_content_returns_unimplemented() {
        let inst = ConnectorInstanceId::new_v4();
        let connector = FakeConnector { instance: inst };
        let cfg = ConnectorConfig::new(ConnectorKind::Notion, AuthKind::OAuth2, ScopeId::new_v4());
        let tok = connector.authenticate(&cfg).unwrap();
        let err = connector
            .fetch_content(&cfg, &tok, &SourceDocumentId::new("doc-1"))
            .unwrap_err();
        match err {
            ConnectorError::Unimplemented(msg) => assert!(
                msg.contains("doc-1"),
                "Unimplemented message should name the document: {msg}"
            ),
            other => panic!("expected Unimplemented, got {other:?}"),
        }
    }

    #[test]
    fn webhook_handler_round_trips() {
        let inst = ConnectorInstanceId::new_v4();
        let connector = FakeConnector { instance: inst };
        let payload = serde_json::json!({
            "type": "document_updated",
            "document_id": "x-1",
            "occurred_at": Utc::now(),
        });
        let body = serde_json::to_vec(&payload).unwrap();
        let evs = connector.handle_webhook_event(&body).unwrap();
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            ConnectorEvent::DocumentUpdated { document_id, .. } => {
                assert_eq!(*document_id, SourceDocumentId::new("x-1"));
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }
}
