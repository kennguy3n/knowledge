//! Integration tests for the Confluence connector — exercises the full
//! sync → incremental → webhook cycle against fixture JSON modelled on
//! Confluence REST `/wiki/rest/api/content` and Atlassian webhooks.

use connector_framework::{
    AuthKind, Connector, ConnectorConfig, ConnectorError, ConnectorEvent, ConnectorInstanceId,
    ConnectorKind, SyncState,
};
use connectors::confluence::{ConfluenceConnector, ConfluenceContentList};
use evidence_store::ScopeId;

const INITIAL_FIXTURE: &str = include_str!("fixtures/confluence_initial.json");
const INCREMENTAL_FIXTURE: &str = include_str!("fixtures/confluence_incremental.json");
const WEBHOOK_REMOVED_FIXTURE: &str = include_str!("fixtures/confluence_webhook_removed.json");

fn cfg() -> ConnectorConfig {
    ConnectorConfig::new(
        ConnectorKind::Confluence,
        AuthKind::OAuth2,
        ScopeId::new_v4(),
    )
}

fn build_connector() -> ConfluenceConnector {
    let initial: ConfluenceContentList =
        serde_json::from_str(INITIAL_FIXTURE).expect("parse initial fixture");
    let incremental: ConfluenceContentList =
        serde_json::from_str(INCREMENTAL_FIXTURE).expect("parse incremental fixture");
    ConfluenceConnector::new(ConnectorInstanceId::new_v4())
        .with_initial_pages(vec![initial])
        .with_incremental_pages(vec![incremental])
}

#[test]
fn full_lifecycle_against_fixture_data() {
    let connector = build_connector();
    let config = cfg();

    let token = connector.authenticate(&config).expect("authenticate");
    assert!(!token.access_token.expose().is_empty());
    assert!(token.scope.contains("confluence-content"));

    let initial = connector
        .initial_sync(&config, &token)
        .expect("initial_sync");
    assert_eq!(initial.events.len(), 2);
    for ev in &initial.events {
        assert!(matches!(ev, ConnectorEvent::DocumentCreated { .. }));
    }
    assert!(initial.next_cursor.is_some(), "RFC-3339 watermark cursor");

    let mut state = SyncState::new(connector.instance);
    state.cursor = initial.next_cursor.clone();
    let incremental = connector
        .incremental_sync(&config, &token, &state)
        .expect("incremental_sync");
    assert_eq!(incremental.events.len(), 1);
    assert!(matches!(
        incremental.events[0],
        ConnectorEvent::DocumentUpdated { .. }
    ));

    let sub = connector
        .subscribe_webhook(&config, &token, "https://substrate.example/confluence")
        .expect("subscribe_webhook");
    assert_eq!(sub.connector, connector.instance);
    assert!(sub.expires_at.is_some());

    let removed = connector
        .handle_webhook_event(WEBHOOK_REMOVED_FIXTURE.as_bytes())
        .expect("handle_webhook_event removed");
    match removed {
        ConnectorEvent::DocumentDeleted { document_id, .. } => {
            assert_eq!(document_id.as_str(), "confluence:page:9");
        }
        other => panic!("expected DocumentDeleted, got {other:?}"),
    }
}

#[test]
fn invalid_json_webhook_returns_error() {
    let connector = build_connector();
    let err = connector.handle_webhook_event(b"<not json>").unwrap_err();
    assert!(matches!(err, ConnectorError::Json(_)));
}

#[test]
fn unknown_webhook_event_returns_webhook_error() {
    let connector = build_connector();
    let body = serde_json::json!({"webhookEvent": "completely_unknown"});
    let err = connector
        .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
        .unwrap_err();
    assert!(matches!(err, ConnectorError::Webhook(_)));
}

#[test]
fn missing_page_body_for_removed_returns_webhook_error() {
    let connector = build_connector();
    let body = serde_json::json!({"webhookEvent": "page_removed"});
    let err = connector
        .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
        .unwrap_err();
    assert!(matches!(err, ConnectorError::Webhook(_)));
}
