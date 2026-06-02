//! Integration tests for the OneDrive connector.
//!
//! Exercises the full `authenticate → initial_sync → incremental_sync
//! → subscribe_webhook → handle_webhook_event` cycle against fixture
//! JSON modeled on Microsoft Graph `/me/drive/root/delta` and Graph
//! subscriptions, served via [`MockHttpTransport`] over the
//! [`HttpTransport`] boundary that production wires to `reqwest`.

use std::sync::Arc;

use chrono::{Duration, Utc};
use connector_framework::{
    AuthKind, Connector, ConnectorConfig, ConnectorError, ConnectorEvent, ConnectorInstanceId,
    ConnectorKind, HttpMethod, HttpTransport, MockHttpTransport, MockResponse, OAuth2CodeExchange,
    OAuth2Token, Result, SourcePermissionLevel, SyncState,
};
use connectors::onedrive::{DeltaResponse, GraphSubscriptionResponse, OneDriveConnector};
use evidence_store::ScopeId;

const INITIAL_FIXTURE: &str = include_str!("fixtures/onedrive_initial.json");
const DELTA_FIXTURE: &str = include_str!("fixtures/onedrive_delta.json");
const WEBHOOK_FIXTURE: &str = include_str!("fixtures/onedrive_webhook_shared.json");

const BASE_URL: &str = "https://api.test/graph";
const DELTA_URL: &str = "https://api.test/graph/v1.0/me/drive/root/delta";

struct FixedOAuth;
impl OAuth2CodeExchange for FixedOAuth {
    fn exchange_code(&self, _config: &ConnectorConfig, _code: &str) -> Result<OAuth2Token> {
        Ok(OAuth2Token::new("graph-access",
            "graph-refresh",
            Utc::now() + Duration::hours(1),
            "Files.Read.All Sites.Read.All",
        ))
    }
}

fn oauth() -> Arc<dyn OAuth2CodeExchange> {
    Arc::new(FixedOAuth)
}

fn cfg() -> ConnectorConfig {
    ConnectorConfig::new(ConnectorKind::OneDrive, AuthKind::OAuth2, ScopeId::new_v4())
        .with_auth_config(serde_json::json!({
            "authorization_code": "demo-code",
            "api_base_url": BASE_URL,
        }))
}

fn install_fixture_responses(transport: &MockHttpTransport) {
    let initial: DeltaResponse =
        serde_json::from_str(INITIAL_FIXTURE).expect("parse initial fixture");
    let incremental_url = initial
        .delta_link
        .clone()
        .expect("initial fixture deltaLink");
    transport.expect(HttpMethod::Get,
        DELTA_URL,
        MockResponse::ok_json(serde_json::to_vec(&initial).unwrap()),
    );
    let delta: DeltaResponse = serde_json::from_str(DELTA_FIXTURE).expect("parse delta fixture");
    transport.expect(HttpMethod::Get,
        &incremental_url,
        MockResponse::ok_json(serde_json::to_vec(&delta).unwrap()),
    );
    transport.expect(HttpMethod::Post,
        "https://api.test/graph/v1.0/subscriptions",
        MockResponse::ok_json(serde_json::to_vec(&GraphSubscriptionResponse {
                id: Some("sub-1".into()),
                expiration_date_time: Some(Utc::now() + Duration::days(2)),
            })
            .unwrap(),
        ),
    );
}

#[test]
fn full_lifecycle_against_fixture_data() {
    let transport = MockHttpTransport::new();
    install_fixture_responses(&transport);
    let transport: Arc<dyn HttpTransport> = Arc::new(transport);
    let connector = OneDriveConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
    let config = cfg();

    let token = connector.authenticate(&config).expect("authenticate");
    assert!(!token.access_token.expose().is_empty());
    assert!(token.scope.contains("Files.Read.All"));
    assert_eq!(token.token_type, "Bearer");

    let initial = connector
        .initial_sync(&config, &token)
        .expect("initial_sync");
    assert_eq!(initial.events.len(), 2, "two items in fixture");
    assert_eq!(initial.next_cursor.as_deref(), Some("graph:delta:42"));

    let mut state = SyncState::new(connector.instance);
    state.cursor.clone_from(&initial.next_cursor);
    let incremental = connector
        .incremental_sync(&config, &token, &state)
        .expect("incremental_sync");
    assert_eq!(incremental.events.len(), 2);
    let updated = incremental
        .events
        .iter()
        .filter(|e| matches!(e, ConnectorEvent::DocumentUpdated { .. }))
        .count();
    let deleted = incremental
        .events
        .iter()
        .filter(|e| matches!(e, ConnectorEvent::DocumentDeleted { .. }))
        .count();
    assert_eq!(updated, 1);
    assert_eq!(deleted, 1);
    assert_eq!(incremental.next_cursor.as_deref(), Some("graph:delta:99"));

    let sub = connector
        .subscribe_webhook(&config, &token, "https://substrate.example/onedrive")
        .expect("subscribe_webhook");
    assert_eq!(sub.connector, connector.instance);
    assert!(sub.expires_at.is_some(), "Graph subs cap at ~3 days");
    assert_eq!(sub.provider_subscription_id.as_deref(), Some("sub-1"));

    let events = connector
        .handle_webhook_event(WEBHOOK_FIXTURE.as_bytes())
        .expect("handle_webhook_event");
    assert_eq!(events.len(), 1, "fixture carries one notification");
    match &events[0] {
        ConnectorEvent::PermissionChanged {
            document_id,
            new_level,
            ..
        } => {
            assert_eq!(document_id.as_str(), "onedrive:item:1");
            assert_eq!(*new_level, Some(SourcePermissionLevel::Write));
        }
        other => panic!("expected permission change, got {other:?}"),
    }
}

#[test]
fn batched_webhook_emits_every_notification() {
    // Regression test for the Devin Review finding that
    // `OneDriveConnector::handle_webhook_event` used to drop every
    // entry past index 0 of the Graph `changeNotification` batch. A
    // single Graph subscription POST routinely carries multiple
    // notifications.
    let transport: Arc<dyn HttpTransport> = Arc::new(MockHttpTransport::new());
    let connector = OneDriveConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
    let body = serde_json::json!({
        "value": [
            {
                "resource": "drive/items/file-a",
                "changeType": "created",
                "subscriptionId": "sub-1",
            },
            {
                "resource": "drive/items/file-b",
                "changeType": "updated",
                "subscriptionId": "sub-1",
            },
            {
                "resource": "drive/items/file-c",
                "changeType": "deleted",
                "subscriptionId": "sub-1",
            }
        ]
    });
    let events = connector
        .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
        .expect("handle_webhook_event");
    assert_eq!(events.len(), 3, "every notification must surface");
    assert!(matches!(events[0], ConnectorEvent::DocumentCreated { .. }));
    assert!(matches!(events[1], ConnectorEvent::DocumentUpdated { .. }));
    assert!(matches!(events[2], ConnectorEvent::DocumentDeleted { .. }));
}

#[test]
fn invalid_json_webhook_returns_error() {
    let transport: Arc<dyn HttpTransport> = Arc::new(MockHttpTransport::new());
    let connector = OneDriveConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
    let err = connector.handle_webhook_event(b"<not-json>").unwrap_err();
    assert!(matches!(err, ConnectorError::Json(_)));
}

#[test]
fn empty_notification_batch_returns_webhook_error() {
    let transport: Arc<dyn HttpTransport> = Arc::new(MockHttpTransport::new());
    let connector = OneDriveConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
    let body = serde_json::json!({"value": []});
    let err = connector
        .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
        .unwrap_err();
    assert!(matches!(err, ConnectorError::Webhook(_)));
}

#[test]
fn unknown_change_type_is_skipped_not_errored() {
    // Regression: an unrecognised `changeType` previously bubbled
    // out of `handle_webhook_event` as `Err` from the `other =>` arm,
    // which discarded every valid notification already queued earlier
    // in the same Graph batch. The handler must skip the unknown
    // entry and keep processing the remainder so a future Graph
    // lifecycle string cannot cause repeated data loss on retries.
    let transport: Arc<dyn HttpTransport> = Arc::new(MockHttpTransport::new());
    let connector = OneDriveConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
    let body = serde_json::json!({
        "value": [
            {
                "resource": "drive/items/file-a",
                "changeType": "created",
                "subscriptionId": "sub-1",
            },
            {
                "resource": "drive/items/abc",
                "changeType": "weird-state",
                "subscriptionId": "sub-1",
            },
            {
                "resource": "drive/items/file-c",
                "changeType": "deleted",
                "subscriptionId": "sub-1",
            }
        ]
    });
    let events = connector
        .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
        .expect("handle_webhook_event");
    assert_eq!(events.len(),
        2,
        "valid notifications on either side of an unknown changeType must still surface",
    );
    assert!(matches!(events[0], ConnectorEvent::DocumentCreated { .. }));
    assert!(matches!(events[1], ConnectorEvent::DocumentDeleted { .. }));
}
