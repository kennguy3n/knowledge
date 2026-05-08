//! Integration tests for the OneDrive connector — exercises the full
//! sync → incremental → webhook cycle against fixture JSON modeled on
//! Microsoft Graph `/drive/root/delta` and Graph subscriptions.

use connector_framework::{
    AuthKind, Connector, ConnectorConfig, ConnectorError, ConnectorEvent, ConnectorInstanceId,
    ConnectorKind, SourcePermissionLevel, SyncState,
};
use connectors::onedrive::{DeltaResponse, OneDriveConnector};
use evidence_store::ScopeId;

const INITIAL_FIXTURE: &str = include_str!("fixtures/onedrive_initial.json");
const DELTA_FIXTURE: &str = include_str!("fixtures/onedrive_delta.json");
const WEBHOOK_FIXTURE: &str = include_str!("fixtures/onedrive_webhook_shared.json");

fn cfg() -> ConnectorConfig {
    ConnectorConfig::new(ConnectorKind::OneDrive, AuthKind::OAuth2, ScopeId::new_v4())
}

fn build_connector() -> OneDriveConnector {
    let initial: DeltaResponse =
        serde_json::from_str(INITIAL_FIXTURE).expect("parse initial fixture");
    let incremental: DeltaResponse =
        serde_json::from_str(DELTA_FIXTURE).expect("parse delta fixture");
    OneDriveConnector::new(ConnectorInstanceId::new_v4())
        .with_initial_pages(vec![initial])
        .with_incremental_pages(vec![incremental])
}

#[test]
fn full_lifecycle_against_fixture_data() {
    let connector = build_connector();
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
    state.cursor = initial.next_cursor.clone();
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

    let event = connector
        .handle_webhook_event(WEBHOOK_FIXTURE.as_bytes())
        .expect("handle_webhook_event");
    match event {
        ConnectorEvent::PermissionChanged {
            document_id,
            new_level,
            ..
        } => {
            assert_eq!(document_id.as_str(), "onedrive:item:1");
            assert_eq!(new_level, Some(SourcePermissionLevel::Write));
        }
        other => panic!("expected permission change, got {other:?}"),
    }
}

#[test]
fn invalid_json_webhook_returns_error() {
    let connector = build_connector();
    let err = connector.handle_webhook_event(b"<not-json>").unwrap_err();
    assert!(matches!(err, ConnectorError::Json(_)));
}

#[test]
fn empty_notification_batch_returns_webhook_error() {
    let connector = build_connector();
    let body = serde_json::json!({"value": []});
    let err = connector
        .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
        .unwrap_err();
    assert!(matches!(err, ConnectorError::Webhook(_)));
}

#[test]
fn unknown_change_type_returns_webhook_error() {
    let connector = build_connector();
    let body = serde_json::json!({
        "value": [{
            "resource": "drive/items/abc",
            "changeType": "weird-state",
            "subscriptionId": "sub-1",
        }]
    });
    let err = connector
        .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
        .unwrap_err();
    assert!(matches!(err, ConnectorError::Webhook(_)));
}
