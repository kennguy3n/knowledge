//! Integration tests for the Figma connector — exercises the full
//! sync → incremental → webhook cycle against fixture JSON modelled on
//! Figma REST API responses.

use connector_framework::{
    AuthKind, Connector, ConnectorConfig, ConnectorError, ConnectorEvent, ConnectorInstanceId,
    ConnectorKind, SyncState,
};
use connectors::figma::{FigmaConnector, FigmaFileResponse};
use evidence_store::ScopeId;

const INITIAL_FIXTURE: &str = include_str!("fixtures/figma_initial.json");
const INCREMENTAL_FIXTURE: &str = include_str!("fixtures/figma_incremental.json");
const WEBHOOK_FIXTURE: &str = include_str!("fixtures/figma_webhook.json");

fn cfg() -> ConnectorConfig {
    ConnectorConfig::new(ConnectorKind::Figma, AuthKind::OAuth2, ScopeId::new_v4())
}

fn build_connector() -> FigmaConnector {
    let initial: FigmaFileResponse =
        serde_json::from_str(INITIAL_FIXTURE).expect("parse initial fixture");
    let incremental: FigmaFileResponse =
        serde_json::from_str(INCREMENTAL_FIXTURE).expect("parse incremental fixture");
    FigmaConnector::new(ConnectorInstanceId::new_v4())
        .with_initial_files(vec![initial])
        .with_incremental_files(vec![incremental])
}

#[test]
fn full_lifecycle_against_fixture_data() {
    let connector = build_connector();
    let config = cfg();

    let token = connector.authenticate(&config).expect("authenticate");
    assert!(!token.access_token.expose().is_empty());
    assert!(token.scope.contains("files:read"));

    let initial = connector
        .initial_sync(&config, &token)
        .expect("initial_sync");
    assert!(!initial.events.is_empty(), "initial sync emits events");
    for ev in &initial.events {
        assert!(matches!(ev, ConnectorEvent::DocumentCreated { .. }));
    }
    assert!(
        initial.next_cursor.is_some(),
        "version watermark cursor seeded",
    );

    let mut state = SyncState::new(connector.instance);
    state.cursor = initial.next_cursor.clone();
    let incremental = connector
        .incremental_sync(&config, &token, &state)
        .expect("incremental_sync");
    assert!(!incremental.events.is_empty());
    assert!(incremental
        .events
        .iter()
        .any(|e| matches!(e, ConnectorEvent::DocumentUpdated { .. })));

    let sub = connector
        .subscribe_webhook(&config, &token, "https://substrate.example/figma")
        .expect("subscribe_webhook");
    assert_eq!(sub.connector, connector.instance);

    let events = connector
        .handle_webhook_event(WEBHOOK_FIXTURE.as_bytes())
        .expect("handle_webhook_event");
    assert_eq!(events.len(), 1, "Figma posts one event per request");
    assert!(matches!(events[0], ConnectorEvent::DocumentUpdated { .. }));
}

#[test]
fn invalid_json_webhook_returns_error() {
    let connector = build_connector();
    let err = connector.handle_webhook_event(b"oops").unwrap_err();
    assert!(matches!(err, ConnectorError::Json(_)));
}

#[test]
fn unknown_webhook_event_returns_webhook_error() {
    let connector = build_connector();
    let body = serde_json::json!({
        "event_type": "TOTALLY_UNKNOWN_EVENT",
        "file_key": "F-DESIGN-1",
    });
    let err = connector
        .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
        .unwrap_err();
    assert!(matches!(err, ConnectorError::Webhook(_)));
}

#[test]
fn missing_file_key_returns_webhook_error() {
    let connector = build_connector();
    let body = serde_json::json!({"event_type": "FILE_VERSION_UPDATE"});
    let err = connector
        .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
        .unwrap_err();
    assert!(matches!(
        err,
        ConnectorError::Json(_) | ConnectorError::Webhook(_)
    ));
}
