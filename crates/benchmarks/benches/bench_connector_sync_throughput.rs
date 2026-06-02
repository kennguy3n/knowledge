//! `bench_connector_sync_throughput` — mock-transport sync of 10K docs.
//!
//! Drives a bench-local [`Connector`] implementation through a full
//! `initial_sync` that paginates 100 pages × 100 documents (10K docs)
//! off a [`MockHttpTransport`] and emits one
//! [`ConnectorEvent::DocumentCreated`] per document.
//!
//! `Throughput::Elements(10_000)` makes Criterion print the document
//! / event-emission rate (events per second).
//!
//! The connector consumes only the public connector-framework
//! surface — `HttpTransport`, `ConnectorEvent`, `SyncRunResult`, the
//! `Connector` trait — and does not touch the framework internals.
//!
//! Run with:
//!
//! ```bash
//! cargo bench -p benchmarks --bench bench_connector_sync_throughput
//! ```

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use std::hint::black_box;

use connector_framework::config::{AuthKind, ConnectorKind};
use connector_framework::{
    Connector, ConnectorConfig, ConnectorError, ConnectorEvent, HttpMethod, HttpTransport,
    MockHttpTransport, MockResponse, OAuth2Token, SourceDocumentId, SyncRunResult, SyncState,
};
use evidence_store::ScopeId;

const PAGES: usize = 100;
const PER_PAGE: usize = 100;
const TOTAL_DOCS: usize = PAGES * PER_PAGE;
const BASE_URL: &str = "https://bench.local/docs";

fn page_url(cursor: &str) -> String {
    format!("{BASE_URL}?cursor={cursor}")
}

/// A minimal connector that walks a paginated document-list endpoint
/// and emits a `DocumentCreated` event per id. Models the steady
/// `GET page -> emit events -> follow cursor` loop every real
/// connector in the workspace runs.
struct DocListConnector {
    transport: Arc<dyn HttpTransport>,
}

impl Connector for DocListConnector {
    fn authenticate(&self, _config: &ConnectorConfig) -> connector_framework::Result<OAuth2Token> {
        Ok(OAuth2Token::new(
            "bench-access",
            "bench-refresh",
            Utc::now() + chrono::Duration::hours(1),
            "documents.read",
        ))
    }

    fn initial_sync(
        &self,
        _config: &ConnectorConfig,
        token: &OAuth2Token,
    ) -> connector_framework::Result<SyncRunResult> {
        let bearer = format!("Bearer {}", token.scope); // scope stands in; access token is secret.
        let now = Utc::now();
        let mut events = Vec::with_capacity(TOTAL_DOCS);
        let mut cursor = "0".to_string();
        loop {
            let url = page_url(&cursor);
            let resp = self
                .transport
                .get(&url, &[("Authorization", bearer.as_str())])?;
            if !resp.is_success() {
                return Err(ConnectorError::Sync(format!(
                    "page fetch failed: {}",
                    resp.status
                )));
            }
            let page: serde_json::Value = serde_json::from_slice(&resp.body)?;
            if let Some(ids) = page.get("ids").and_then(serde_json::Value::as_array) {
                for id in ids {
                    if let Some(id) = id.as_str() {
                        events.push(ConnectorEvent::DocumentCreated {
                            document_id: SourceDocumentId::new(id),
                            occurred_at: now,
                        });
                    }
                }
            }
            match page.get("next").and_then(serde_json::Value::as_str) {
                Some(next) => cursor = next.to_string(),
                None => break,
            }
        }
        Ok(SyncRunResult {
            events,
            next_cursor: None,
        })
    }

    fn incremental_sync(
        &self,
        _config: &ConnectorConfig,
        _token: &OAuth2Token,
        _state: &SyncState,
    ) -> connector_framework::Result<SyncRunResult> {
        Ok(SyncRunResult {
            events: Vec::new(),
            next_cursor: None,
        })
    }

    fn subscribe_webhook(
        &self,
        _config: &ConnectorConfig,
        _token: &OAuth2Token,
        _callback_url: &str,
    ) -> connector_framework::Result<connector_framework::WebhookSubscription> {
        // The bench connector models polling sync only; push
        // subscriptions are out of scope.
        Err(ConnectorError::Webhook(
            "bench connector does not support webhooks".into(),
        ))
    }

    fn handle_webhook_event(
        &self,
        _body: &[u8],
    ) -> connector_framework::Result<Vec<ConnectorEvent>> {
        Err(ConnectorError::Webhook(
            "bench connector does not support webhooks".into(),
        ))
    }
}

/// Precompute the 100 page bodies once so the per-iteration setup
/// only pays to register clones into a fresh transport (the mock
/// consumes each canned response, so every iteration needs a fresh
/// transport).
fn page_bodies() -> Vec<(String, Vec<u8>)> {
    (0..PAGES)
        .map(|p| {
            let ids: Vec<String> = (0..PER_PAGE)
                .map(|j| format!("doc-{}", p * PER_PAGE + j))
                .collect();
            let next = if p + 1 < PAGES {
                serde_json::Value::String((p + 1).to_string())
            } else {
                serde_json::Value::Null
            };
            let body = serde_json::json!({ "ids": ids, "next": next });
            (
                page_url(&p.to_string()),
                serde_json::to_vec(&body).expect("serialize page"),
            )
        })
        .collect()
}

fn build_connector(pages: &[(String, Vec<u8>)]) -> DocListConnector {
    let transport = MockHttpTransport::new();
    for (url, body) in pages {
        transport.expect(
            HttpMethod::Get,
            url.clone(),
            MockResponse::ok_json(body.clone()),
        );
    }
    DocListConnector {
        transport: Arc::new(transport),
    }
}

fn bench_connector_sync_throughput(c: &mut Criterion) {
    let pages = page_bodies();
    let config = ConnectorConfig::new(ConnectorKind::GitHub, AuthKind::OAuth2, ScopeId::new_v4());
    let token = OAuth2Token::new(
        "bench-access",
        "bench-refresh",
        Utc::now() + chrono::Duration::hours(1),
        "documents.read",
    );

    let mut group = c.benchmark_group("connector/sync_10k_docs");
    group.throughput(Throughput::Elements(TOTAL_DOCS as u64));
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(15));

    group.bench_function("initial_sync_mock_transport", |b| {
        b.iter_with_setup(
            || build_connector(&pages),
            |connector| {
                let result = connector
                    .initial_sync(black_box(&config), black_box(&token))
                    .expect("initial_sync");
                assert_eq!(result.events.len(), TOTAL_DOCS);
                black_box(result.events.len());
            },
        );
    });
    group.finish();
}

criterion_group!(connector_benches, bench_connector_sync_throughput);
criterion_main!(connector_benches);
