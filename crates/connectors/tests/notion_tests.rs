//! Integration tests for the Notion connector — a pure poll-based
//! source. Webhook calls intentionally return errors tagged
//! `polling-only`.

use connector_framework::{
    AuthKind, Connector, ConnectorConfig, ConnectorError, ConnectorEvent, ConnectorInstanceId,
    ConnectorKind, SyncState,
};
use connectors::notion::{NotionConnector, NotionSearchResponse};
use evidence_store::ScopeId;

const SEARCH_FIXTURE: &str = include_str!("fixtures/notion_search.json");
const QUERY_FIXTURE: &str = include_str!("fixtures/notion_query.json");

fn cfg() -> ConnectorConfig {
    ConnectorConfig::new(ConnectorKind::Notion, AuthKind::OAuth2, ScopeId::new_v4())
}

fn build_connector() -> NotionConnector {
    let initial: NotionSearchResponse =
        serde_json::from_str(SEARCH_FIXTURE).expect("parse /search fixture");
    let incremental: NotionSearchResponse =
        serde_json::from_str(QUERY_FIXTURE).expect("parse /query fixture");
    NotionConnector::new(ConnectorInstanceId::new_v4())
        .with_initial_pages(vec![initial])
        .with_incremental_pages(vec![incremental])
}

#[test]
fn full_lifecycle_against_fixture_data() {
    let connector = build_connector();
    let config = cfg();

    let token = connector.authenticate(&config).expect("authenticate");
    assert!(!token.access_token.expose().is_empty());
    assert!(token.scope.contains("read_content"));

    let initial = connector
        .initial_sync(&config, &token)
        .expect("initial_sync");
    assert_eq!(initial.events.len(), 2, "page + database in fixture");
    assert!(
        initial.next_cursor.is_some(),
        "cursor seeded from last_edited_time",
    );

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
    assert_eq!(updated, 1, "edited page");
    assert_eq!(deleted, 1, "archived page maps to delete");
}

#[test]
fn subscribe_webhook_is_polling_only() {
    let connector = build_connector();
    let config = cfg();
    let token = connector.authenticate(&config).expect("authenticate");
    let err = connector
        .subscribe_webhook(&config, &token, "https://substrate.example/notion")
        .unwrap_err();
    match err {
        ConnectorError::Webhook(msg) => assert!(msg.to_lowercase().contains("polling")),
        other => panic!("expected webhook polling-only error, got {other:?}"),
    }
}

#[test]
fn handle_webhook_event_is_polling_only() {
    let connector = build_connector();
    let body = serde_json::json!({"object": "page", "id": "x"});
    let err = connector
        .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
        .unwrap_err();
    assert!(matches!(err, ConnectorError::Webhook(_)));
}

#[test]
fn invalid_json_handle_webhook_event_still_polling_only() {
    let connector = build_connector();
    // Notion's polling-only webhook handler short-circuits regardless
    // of body shape — the error must still surface as Webhook.
    let err = connector.handle_webhook_event(b"invalid").unwrap_err();
    assert!(matches!(err, ConnectorError::Webhook(_)));
}
