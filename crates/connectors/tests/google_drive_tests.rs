//! Integration tests for the Google Drive connector.
//!
//! Exercises the full sync → incremental → webhook cycle against
//! fixture JSON in `tests/fixtures/google_drive_*.json` to mirror the
//! shape of real Drive API responses.

use connector_framework::{
    AuthKind, Connector, ConnectorConfig, ConnectorError, ConnectorEvent, ConnectorInstanceId,
    ConnectorKind, SourcePermissionLevel, SyncState,
};
use connectors::google_drive::{GoogleDriveChangeList, GoogleDriveConnector, GoogleDriveFileList};
use evidence_store::ScopeId;

const FILES_LIST_FIXTURE: &str = include_str!("fixtures/google_drive_files_list.json");
const CHANGES_FIXTURE: &str = include_str!("fixtures/google_drive_changes.json");
const PUSH_FIXTURE: &str = include_str!("fixtures/google_drive_push.json");

fn cfg() -> ConnectorConfig {
    ConnectorConfig::new(
        ConnectorKind::GoogleDrive,
        AuthKind::OAuth2,
        ScopeId::new_v4(),
    )
}

fn build_connector() -> GoogleDriveConnector {
    let initial: GoogleDriveFileList =
        serde_json::from_str(FILES_LIST_FIXTURE).expect("parse files.list fixture");
    let incremental: GoogleDriveChangeList =
        serde_json::from_str(CHANGES_FIXTURE).expect("parse changes fixture");
    GoogleDriveConnector::new(ConnectorInstanceId::new_v4())
        .with_initial_pages(vec![initial])
        .with_incremental_pages(vec![incremental])
}

#[test]
fn full_lifecycle_against_fixture_data() {
    let connector = build_connector();
    let config = cfg();

    // 1. Authenticate.
    let token = connector.authenticate(&config).expect("authenticate");
    assert!(
        !token.access_token.expose().is_empty(),
        "access_token populated",
    );
    assert!(
        !token.refresh_token.expose().is_empty(),
        "refresh_token populated",
    );
    assert!(
        token.scope.contains("drive"),
        "scope should mention drive: {}",
        token.scope
    );
    assert_eq!(token.token_type, "Bearer");

    // 2. Initial sync — fixture has 2 files, both created.
    let initial = connector
        .initial_sync(&config, &token)
        .expect("initial_sync");
    assert_eq!(initial.events.len(), 2, "two files in fixture → two events");
    for ev in &initial.events {
        assert!(matches!(ev, ConnectorEvent::DocumentCreated { .. }));
    }
    assert_eq!(
        initial.next_cursor.as_deref(),
        Some("drive:start:42"),
        "next_cursor should be the newStartPageToken"
    );

    // 3. Incremental sync — fixture has 1 update + 1 removal.
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
    assert_eq!(updated, 1, "one updated document");
    assert_eq!(deleted, 1, "one deleted document");
    assert_eq!(
        incremental.next_cursor.as_deref(),
        Some("drive:start:99"),
        "newStartPageToken advances"
    );

    // 4. Subscribe webhook.
    let sub = connector
        .subscribe_webhook(&config, &token, "https://substrate.example/hooks/drive")
        .expect("subscribe_webhook");
    assert_eq!(sub.connector, connector.instance);
    assert_eq!(sub.callback_url, "https://substrate.example/hooks/drive");
    assert!(!sub.secret.expose().is_empty());
    assert!(sub.expires_at.is_some(), "Drive channels carry a TTL");

    // 5. Handle webhook event — permission change fixture.
    let events = connector
        .handle_webhook_event(PUSH_FIXTURE.as_bytes())
        .expect("handle_webhook_event");
    assert_eq!(events.len(), 1, "Drive push notifications carry one event");
    match &events[0] {
        ConnectorEvent::PermissionChanged {
            document_id,
            new_level,
            ..
        } => {
            assert_eq!(document_id.as_str(), "drive:file:1");
            assert_eq!(*new_level, Some(SourcePermissionLevel::Write));
        }
        other => panic!("expected permission change, got {other:?}"),
    }
}

#[test]
fn invalid_json_webhook_returns_error() {
    let connector = build_connector();
    let err = connector
        .handle_webhook_event(b"{not valid json")
        .expect_err("invalid JSON should fail");
    assert!(matches!(err, ConnectorError::Json(_)));
}

#[test]
fn unknown_resource_state_returns_webhook_error() {
    let connector = build_connector();
    let body = serde_json::json!({
        "resourceId": "drive:file:1",
        "resourceState": "totally-unknown-state",
    });
    let err = connector
        .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
        .expect_err("unknown state should fail");
    assert!(matches!(err, ConnectorError::Webhook(_)));
}

#[test]
fn missing_required_field_returns_error() {
    let connector = build_connector();
    // Missing required `resourceState` field.
    let body = serde_json::json!({"resourceId": "drive:file:1"});
    let err = connector
        .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
        .expect_err("missing field should fail");
    assert!(matches!(err, ConnectorError::Json(_)));
}
