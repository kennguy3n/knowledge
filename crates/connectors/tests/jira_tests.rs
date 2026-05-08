//! Integration tests for the Jira connector — exercises the full
//! sync → incremental → webhook cycle against fixture JSON modelled
//! on Jira REST API v3 (`/rest/api/3/search`) and Jira webhooks.

use connector_framework::{
    AuthKind, Connector, ConnectorConfig, ConnectorError, ConnectorEvent, ConnectorInstanceId,
    ConnectorKind, SourcePermissionLevel, SyncState,
};
use connectors::jira::{JiraConnector, JiraSearchResponse};
use evidence_store::ScopeId;

const INITIAL_FIXTURE: &str = include_str!("fixtures/jira_initial.json");
const INCREMENTAL_FIXTURE: &str = include_str!("fixtures/jira_incremental.json");
const WEBHOOK_CREATED_FIXTURE: &str = include_str!("fixtures/jira_webhook_created.json");
const WEBHOOK_PERMISSION_FIXTURE: &str = include_str!("fixtures/jira_webhook_permission.json");

fn cfg() -> ConnectorConfig {
    ConnectorConfig::new(ConnectorKind::Jira, AuthKind::OAuth2, ScopeId::new_v4())
}

fn build_connector() -> JiraConnector {
    let initial: JiraSearchResponse =
        serde_json::from_str(INITIAL_FIXTURE).expect("parse initial fixture");
    let incremental: JiraSearchResponse =
        serde_json::from_str(INCREMENTAL_FIXTURE).expect("parse incremental fixture");
    JiraConnector::new(ConnectorInstanceId::new_v4())
        .with_initial_pages(vec![initial])
        .with_incremental_pages(vec![incremental])
}

#[test]
fn full_lifecycle_against_fixture_data() {
    let connector = build_connector();
    let config = cfg();

    let token = connector.authenticate(&config).expect("authenticate");
    assert!(!token.access_token.expose().is_empty());
    assert!(token.scope.contains("jira"));

    let initial = connector
        .initial_sync(&config, &token)
        .expect("initial_sync");
    assert_eq!(initial.events.len(), 2, "two issues in fixture");
    for ev in &initial.events {
        assert!(matches!(ev, ConnectorEvent::DocumentCreated { .. }));
    }
    assert!(initial.next_cursor.is_some(), "cursor seeded from updated");

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
        .subscribe_webhook(&config, &token, "https://substrate.example/jira")
        .expect("subscribe_webhook");
    assert_eq!(sub.connector, connector.instance);
    assert_eq!(sub.callback_url, "https://substrate.example/jira");

    let created = connector
        .handle_webhook_event(WEBHOOK_CREATED_FIXTURE.as_bytes())
        .expect("handle_webhook_event created");
    assert_eq!(created.len(), 1, "Jira posts one event per request");
    match &created[0] {
        ConnectorEvent::DocumentCreated { document_id, .. } => {
            assert_eq!(document_id.as_str(), "PROJ-103");
        }
        other => panic!("expected DocumentCreated, got {other:?}"),
    }

    let permission = connector
        .handle_webhook_event(WEBHOOK_PERMISSION_FIXTURE.as_bytes())
        .expect("handle_webhook_event permission");
    assert_eq!(permission.len(), 1, "Jira posts one event per request");
    match &permission[0] {
        ConnectorEvent::PermissionChanged {
            document_id,
            new_level,
            ..
        } => {
            assert_eq!(document_id.as_str(), "PROJ-101");
            assert_eq!(*new_level, Some(SourcePermissionLevel::Admin));
        }
        other => panic!("expected PermissionChanged, got {other:?}"),
    }
}

#[test]
fn invalid_json_webhook_returns_error() {
    let connector = build_connector();
    let err = connector.handle_webhook_event(b"not-a-json").unwrap_err();
    assert!(matches!(err, ConnectorError::Json(_)));
}

#[test]
fn unknown_webhook_event_returns_webhook_error() {
    let connector = build_connector();
    let body = serde_json::json!({"webhookEvent": "jira:weird"});
    let err = connector
        .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
        .unwrap_err();
    assert!(matches!(err, ConnectorError::Webhook(_)));
}

#[test]
fn missing_issue_body_returns_webhook_error() {
    let connector = build_connector();
    let body = serde_json::json!({"webhookEvent": "jira:issue_created"});
    let err = connector
        .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
        .unwrap_err();
    assert!(matches!(err, ConnectorError::Webhook(_)));
}
