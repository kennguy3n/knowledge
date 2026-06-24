//! `bench_webhook_processing` — webhook event-decode latency.
//!
//! Measures the time to decode and convert inbound webhook payloads
//! (Graph change-notification batches and Gmail Pub/Sub envelopes)
//! into `ConnectorEvent` vectors via the real `EmailConnector`
//! `handle_webhook_event` path.
//!
//! Exercises two payload shapes:
//!
//! * **graph_batch_50** — a 50-notification Graph batch (the largest
//!   batch Microsoft Graph delivers in a single POST).
//! * **gmail_pubsub** — a single Gmail Pub/Sub push with 10 message
//!   ids pre-resolved.
//!
//! Run with:
//!
//! ```bash
//! cargo bench -p benchmarks --bench bench_webhook_processing
//! ```

use std::sync::Arc;

use chrono::{Duration, Utc};
use connector_framework::{
    Connector, ConnectorInstanceId, MockHttpTransport, OAuth2CodeExchange, OAuth2Token,
};
use connectors::email::EmailConnector;
use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use std::hint::black_box;

struct FixedOAuth;
impl OAuth2CodeExchange for FixedOAuth {
    fn exchange_code(
        &self,
        _config: &connector_framework::ConnectorConfig,
        _code: &str,
    ) -> connector_framework::Result<OAuth2Token> {
        Ok(OAuth2Token::new(
            "bench-access",
            "bench-refresh",
            Utc::now() + Duration::hours(1),
            "Mail.Read",
        ))
    }
}

fn connector() -> EmailConnector {
    let transport = MockHttpTransport::new();
    EmailConnector::new(
        ConnectorInstanceId::new_v4(),
        Arc::new(transport),
        Arc::new(FixedOAuth),
    )
}

fn graph_batch_payload(count: usize) -> Vec<u8> {
    let notifications: Vec<serde_json::Value> = (0..count)
        .map(|i| {
            serde_json::json!({
                "changeType": "created",
                "resource": format!("/me/messages/msg-{i}"),
                "resourceData": {"id": format!("msg-{i}")}
            })
        })
        .collect();
    serde_json::to_vec(&serde_json::json!({ "value": notifications })).unwrap()
}

fn gmail_pubsub_payload(count: usize) -> Vec<u8> {
    let ids: Vec<String> = (0..count).map(|i| format!("gmail-msg-{i}")).collect();
    serde_json::to_vec(&serde_json::json!({
        "emailAddress": "user@example.com",
        "historyId": 12345,
        "messageIds": ids
    }))
    .unwrap()
}

fn bench_webhook_processing(c: &mut Criterion) {
    let conn = connector();
    let graph_payload = graph_batch_payload(50);
    let gmail_payload = gmail_pubsub_payload(10);

    let mut group = c.benchmark_group("connector/webhook");

    group.throughput(Throughput::Elements(50));
    group.bench_function("graph_batch_50", |b| {
        b.iter(|| {
            let evs = conn
                .handle_webhook_event(black_box(&graph_payload))
                .expect("decode graph batch");
            assert_eq!(evs.len(), 50);
            black_box(evs.len());
        });
    });

    group.throughput(Throughput::Elements(10));
    group.bench_function("gmail_pubsub_10", |b| {
        b.iter(|| {
            let evs = conn
                .handle_webhook_event(black_box(&gmail_payload))
                .expect("decode gmail pubsub");
            assert_eq!(evs.len(), 10);
            black_box(evs.len());
        });
    });

    group.finish();
}

criterion_group!(webhook_benches, bench_webhook_processing);
criterion_main!(webhook_benches);
