//! Integration tests for the Google Drive connector.
//!
//! Exercises the full sync → incremental → webhook cycle against
//! fixture JSON in `tests/fixtures/google_drive_*.json` to mirror the
//! shape of real Drive API responses, using the `MockHttpTransport`
//! to serve the fixtures over the [`HttpTransport`] boundary that
//! production wires to `reqwest`.
//!
//! These tests live alongside the in-module unit tests — the unit
//! tests pin individual API behaviours (pagination loop guard,
//! webhook parsing, etc.), while this file drives the full
//! `authenticate → initial_sync → incremental_sync → subscribe_webhook
//! → handle_webhook_event` lifecycle end-to-end.

use std::sync::Arc;

use chrono::{Duration, Utc};
use connector_framework::{
    bearer_get_json, percent_encode_form_component, AuthKind, Connector, ConnectorConfig,
    ConnectorError, ConnectorEvent, ConnectorInstanceId, ConnectorKind, HttpMethod, HttpTransport,
    MockHttpTransport, MockResponse, OAuth2CodeExchange, OAuth2Token, Result,
    SourcePermissionLevel, SyncState,
};
use connectors::google_drive::{
    GoogleDriveChangeList, GoogleDriveConnector, GoogleDriveFileList, GoogleDriveStartPageToken,
    GoogleDriveWatchResponse, DEFAULT_PAGE_SIZE,
};
use evidence_store::ScopeId;

// Quiet the unused-import warning — these helpers exist so the test
// file mirrors the connector's URL-building behaviour and we don't
// drift from production wiring.
#[allow(dead_code)]
fn ensure_helpers_linked() {
    let _ = bearer_get_json::<serde_json::Value>;
    let _ = percent_encode_form_component;
}

const FILES_LIST_FIXTURE: &str = include_str!("fixtures/google_drive_files_list.json");
const CHANGES_FIXTURE: &str = include_str!("fixtures/google_drive_changes.json");
const PUSH_FIXTURE: &str = include_str!("fixtures/google_drive_push.json");

const BASE_URL: &str = "https://api.test/google";
const FILE_LIST_FIELDS_MASK: &str =
    "nextPageToken,files(id,name,mimeType,trashed,modifiedTime,createdTime)";
const CHANGE_LIST_FIELDS_MASK: &str = "nextPageToken,newStartPageToken,\
     changes(fileId,kind,removed,time,file(id,name,mimeType,trashed,modifiedTime,createdTime))";

struct FixedOAuth;
impl OAuth2CodeExchange for FixedOAuth {
    fn exchange_code(&self, _config: &ConnectorConfig, _code: &str) -> Result<OAuth2Token> {
        Ok(OAuth2Token::new(
            "drive-access",
            "drive-refresh",
            Utc::now() + Duration::hours(1),
            "https://www.googleapis.com/auth/drive.readonly",
        ))
    }
}

fn oauth() -> Arc<dyn OAuth2CodeExchange> {
    Arc::new(FixedOAuth)
}

fn cfg() -> ConnectorConfig {
    ConnectorConfig::new(
        ConnectorKind::GoogleDrive,
        AuthKind::OAuth2,
        ScopeId::new_v4(),
    )
    .with_auth_config(serde_json::json!({
        "authorization_code": "demo-code",
        "api_base_url": BASE_URL,
        "start_page_token": "watch-start-1",
        "channel_id": "chan-77",
    }))
}

fn files_list_url(page_token: Option<&str>) -> String {
    let mut url = format!(
        "{BASE_URL}/drive/v3/files?pageSize={}&q={}&fields={}",
        DEFAULT_PAGE_SIZE,
        percent_encode_form_component("trashed = false"),
        percent_encode_form_component(FILE_LIST_FIELDS_MASK),
    );
    if let Some(tok) = page_token {
        url.push_str("&pageToken=");
        url.push_str(&percent_encode_form_component(tok));
    }
    url
}

fn changes_list_url(page_token: &str) -> String {
    format!(
        "{BASE_URL}/drive/v3/changes?pageToken={}&pageSize={}&includeRemoved=true&fields={}",
        percent_encode_form_component(page_token),
        DEFAULT_PAGE_SIZE,
        percent_encode_form_component(CHANGE_LIST_FIELDS_MASK),
    )
}

fn install_fixture_responses(transport: &MockHttpTransport) {
    // Reshape the fixture-shaped files page into the live wire format
    // so the test exercises real cursor pagination via the mock.
    let files: GoogleDriveFileList =
        serde_json::from_str(FILES_LIST_FIXTURE).expect("parse files.list fixture");
    transport.expect(
        HttpMethod::Get,
        files_list_url(None),
        MockResponse::ok_json(serde_json::to_vec(&files).unwrap()),
    );
    // Anchor the changes feed.
    transport.expect(
        HttpMethod::Get,
        format!("{BASE_URL}/drive/v3/changes/startPageToken"),
        MockResponse::ok_json(
            serde_json::to_vec(&GoogleDriveStartPageToken {
                start_page_token: Some("drive:start:42".into()),
            })
            .unwrap(),
        ),
    );
    let changes: GoogleDriveChangeList =
        serde_json::from_str(CHANGES_FIXTURE).expect("parse changes fixture");
    transport.expect(
        HttpMethod::Get,
        changes_list_url("drive:start:42"),
        MockResponse::ok_json(serde_json::to_vec(&changes).unwrap()),
    );
    // Drive's watch endpoint.
    transport.expect(
        HttpMethod::Post,
        format!(
            "{BASE_URL}/drive/v3/changes/watch?pageToken={}",
            percent_encode_form_component("watch-start-1"),
        ),
        MockResponse::ok_json(
            serde_json::to_vec(&GoogleDriveWatchResponse {
                id: Some("chan-77".into()),
                resource_id: Some("res-1".into()),
                expiration: Some((Utc::now() + Duration::days(7)).timestamp_millis()),
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
    let connector = GoogleDriveConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
    let config = cfg();

    // 1. Authenticate.
    let token = connector.authenticate(&config).expect("authenticate");
    assert!(
        !token.access_token.expose().is_empty(),
        "access_token populated",
    );
    assert!(
        token
            .refresh_token
            .as_ref()
            .is_some_and(|rt| !rt.expose().is_empty()),
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
        "next_cursor should be the startPageToken anchor",
    );

    // 3. Incremental sync — fixture has 1 update + 1 removal.
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
    assert_eq!(updated, 1, "one updated document");
    assert_eq!(deleted, 1, "one deleted document");
    assert_eq!(
        incremental.next_cursor.as_deref(),
        Some("drive:start:99"),
        "newStartPageToken advances",
    );

    // 4. Subscribe webhook.
    let sub = connector
        .subscribe_webhook(&config, &token, "https://substrate.example/hooks/drive")
        .expect("subscribe_webhook");
    assert_eq!(sub.connector, connector.instance);
    assert_eq!(sub.callback_url, "https://substrate.example/hooks/drive");
    assert!(!sub.secret.expose().is_empty());
    assert!(sub.expires_at.is_some(), "Drive channels carry a TTL");
    assert_eq!(
        sub.provider_subscription_id.as_deref(),
        Some("chan-77:res-1"),
        "channel id + resource id captured for revocation",
    );

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
    let transport = MockHttpTransport::new();
    let transport: Arc<dyn HttpTransport> = Arc::new(transport);
    let connector = GoogleDriveConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
    let err = connector
        .handle_webhook_event(b"{not valid json")
        .expect_err("invalid JSON should fail");
    assert!(matches!(err, ConnectorError::Json(_)));
}

#[test]
fn unknown_resource_state_returns_webhook_error() {
    let transport = MockHttpTransport::new();
    let transport: Arc<dyn HttpTransport> = Arc::new(transport);
    let connector = GoogleDriveConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
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
    let transport = MockHttpTransport::new();
    let transport: Arc<dyn HttpTransport> = Arc::new(transport);
    let connector = GoogleDriveConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
    // Missing required `resourceState` field.
    let body = serde_json::json!({"resourceId": "drive:file:1"});
    let err = connector
        .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
        .expect_err("missing field should fail");
    assert!(matches!(err, ConnectorError::Json(_)));
}
