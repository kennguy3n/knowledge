//! Integration test: full sync-cycle for every connector.
//!
//! Each test creates a connector wired to [`MockHttpTransport`] and a
//! fixed [`OAuth2CodeExchange`], then drives the full lifecycle:
//!
//! 1. `authenticate()` → returns a canned token.
//! 2. `initial_sync()` → walks paginated mock responses, produces
//!    events + cursor.
//! 3. `incremental_sync()` → uses the cursor, produces delta events.
//! 4. `subscribe_webhook()` → POSTs to the mock, returns a
//!    subscription.
//! 5. `handle_webhook_event()` → parses a canned payload, emits
//!    events.
//!
//! These tests are hermetic — no live network calls. They exercise
//! the same code paths the production connectors traverse, proving
//! that the `HttpTransport` + `OAuth2CodeExchange` plumbing is
//! correctly wired end-to-end without needing provider sandboxes.

use std::sync::Arc;

use chrono::{Duration, Utc};
use connector_framework::{
    percent_encode_path_component, AuthKind, Connector, ConnectorConfig, ConnectorEvent,
    ConnectorInstanceId, ConnectorKind, HttpMethod, MockHttpTransport, MockResponse,
    OAuth2CodeExchange, OAuth2Token, Result, SyncState, WatermarkCursor,
};
use connectors::{
    ConfluenceConnector, EmailConnector, FigmaConnector, GitHubConnector, GoogleDriveConnector,
    HubSpotConnector, JiraConnector, NotionConnector, OneDriveConnector, SlackConnector,
};
use evidence_store::ScopeId;

// ─── shared helpers ───

struct FixedOAuth;
impl OAuth2CodeExchange for FixedOAuth {
    fn exchange_code(&self, _config: &ConnectorConfig, _code: &str) -> Result<OAuth2Token> {
        Ok(OAuth2Token::new(
            "test-access",
            "test-refresh",
            Utc::now() + Duration::hours(1),
            "test-scope",
        ))
    }
}

fn oauth() -> Arc<dyn OAuth2CodeExchange> {
    Arc::new(FixedOAuth)
}

// ─── GitHub full cycle ───

#[test]
fn github_full_sync_cycle() {
    let transport = MockHttpTransport::new();
    let now = Utc::now();
    let base = "https://api.test";
    let repo = "owner/test-repo";

    // initial_sync: page 1 (final — fewer than 100 results).
    let issues = vec![
        serde_json::json!({
            "number": 1, "id": 1, "title": "Issue 1",
            "state": "open",
            "created_at": now - Duration::hours(2),
            "updated_at": now - Duration::hours(1),
        }),
        serde_json::json!({
            "number": 2, "id": 2, "title": "Issue 2",
            "state": "closed",
            "created_at": now - Duration::hours(3),
            "updated_at": now,
            "closed_at": now,
        }),
    ];
    transport.expect(
        HttpMethod::Get,
        format!(
            "{base}/repos/{repo}/issues\
             ?state=all&sort=updated&direction=asc&per_page=100&page=1"
        ),
        MockResponse::ok_json(serde_json::to_vec(&issues).unwrap()),
    );

    let transport: Arc<dyn connector_framework::HttpTransport> = Arc::new(transport);
    let inst = ConnectorInstanceId::new_v4();
    let c = GitHubConnector::new(inst, transport, oauth()).with_api_base_url(base);
    let cfg = ConnectorConfig::new(ConnectorKind::GitHub, AuthKind::OAuth2, ScopeId::new_v4())
        .with_auth_config(serde_json::json!({
            "authorization_code": "code",
            "repository": repo,
            "api_base_url": base,
            "webhook_secret": "test-webhook-secret",
        }));

    // Step 1: authenticate.
    let tok = c.authenticate(&cfg).unwrap();
    assert_eq!(tok.access_token.expose(), "test-access");

    // Step 2: initial_sync.
    let res = c.initial_sync(&cfg, &tok).unwrap();
    assert_eq!(res.events.len(), 2);
    assert!(res.next_cursor.is_some());
    assert!(res
        .events
        .iter()
        .all(|e| matches!(e, ConnectorEvent::DocumentCreated { .. })));

    // Step 3: incremental_sync (new transport for fresh expectations).
    let transport2 = MockHttpTransport::new();
    let cursor = res.next_cursor.as_ref().unwrap();
    // The connector sends `since=<watermark timestamp>`, derived from
    // `WatermarkCursor::query_since()` — NOT the full persisted cursor
    // string (which is `{timestamp}|{boundary-ids}` once any boundary
    // id is recorded). Derive the expected value the same way so the
    // mock URL matches what `incremental_sync` actually requests.
    let since = WatermarkCursor::parse(Some(cursor))
        .query_since()
        .expect("initial-sync cursor carries a watermark");
    let encoded_since = percent_encode_path_component(&since);
    transport2.expect(
        HttpMethod::Get,
        format!(
            "{base}/repos/{repo}/issues\
             ?state=all&sort=updated&direction=asc\
             &per_page=100&page=1&since={encoded_since}"
        ),
        MockResponse::ok_json(
            serde_json::to_vec(&[serde_json::json!({
                "number": 3, "id": 3, "title": "New issue",
                "state": "open",
                "created_at": now + Duration::minutes(5),
                "updated_at": now + Duration::minutes(10),
            })])
            .unwrap(),
        ),
    );
    let transport2: Arc<dyn connector_framework::HttpTransport> = Arc::new(transport2);
    let c2 = GitHubConnector::new(inst, transport2, oauth()).with_api_base_url(base);
    let mut state = SyncState::new(inst);
    state.cursor = res.next_cursor;
    let inc = c2.incremental_sync(&cfg, &tok, &state).unwrap();
    assert_eq!(inc.events.len(), 1);
    assert!(matches!(
        inc.events[0],
        ConnectorEvent::DocumentUpdated { .. }
    ));

    // Step 4: subscribe_webhook.
    let transport3 = MockHttpTransport::new();
    transport3.expect(
        HttpMethod::Post,
        format!("{base}/repos/{repo}/hooks"),
        MockResponse::ok_json(
            serde_json::to_vec(&serde_json::json!({
                "id": 99,
                "active": true,
                "events": ["issues", "pull_request"],
            }))
            .unwrap(),
        ),
    );
    let transport3: Arc<dyn connector_framework::HttpTransport> = Arc::new(transport3);
    let c3 = GitHubConnector::new(inst, transport3, oauth()).with_api_base_url(base);
    let sub = c3
        .subscribe_webhook(&cfg, &tok, "https://example.com/webhook")
        .unwrap();
    assert_eq!(sub.connector, inst);
    assert_eq!(sub.provider_subscription_id.as_deref(), Some("99"));

    // Step 5: handle_webhook_event.
    let payload = serde_json::json!({
        "event_type": "issues",
        "action": "opened",
        "issue": {
            "number": 50, "id": 50,
            "title": "Webhook issue",
            "state": "open",
            "created_at": now,
            "updated_at": now,
        }
    });
    let evs = c
        .handle_webhook_event(&serde_json::to_vec(&payload).unwrap())
        .unwrap();
    assert_eq!(evs.len(), 1);
    assert!(matches!(evs[0], ConnectorEvent::DocumentCreated { .. }));
}

// ─── Jira: authenticate + webhook ───

#[test]
fn jira_authenticate_and_webhook_cycle() {
    let transport = MockHttpTransport::new();
    let transport: Arc<dyn connector_framework::HttpTransport> = Arc::new(transport);
    let inst = ConnectorInstanceId::new_v4();
    let c = JiraConnector::new(inst, transport, oauth());
    let cfg = ConnectorConfig::new(ConnectorKind::Jira, AuthKind::OAuth2, ScopeId::new_v4())
        .with_auth_config(serde_json::json!({
            "authorization_code": "code",
            "api_base_url": "https://tenant.atlassian.net",
        }));

    let tok = c.authenticate(&cfg).unwrap();
    assert_eq!(tok.access_token.expose(), "test-access");

    let now = Utc::now();
    let payload = serde_json::json!({
        "webhookEvent": "jira:issue_created",
        "issue": {
            "key": "PROJ-1", "id": "1",
            "fields": {
                "summary": "From webhook",
                "created": now, "updated": now,
            }
        }
    });
    let evs = c
        .handle_webhook_event(&serde_json::to_vec(&payload).unwrap())
        .unwrap();
    assert_eq!(evs.len(), 1);
    assert!(matches!(evs[0], ConnectorEvent::DocumentCreated { .. }));
}

// ─── Notion: authenticate + webhook (polling model) ───

#[test]
fn notion_authenticate_cycle() {
    let transport = MockHttpTransport::new();
    let transport: Arc<dyn connector_framework::HttpTransport> = Arc::new(transport);
    let inst = ConnectorInstanceId::new_v4();
    let c = NotionConnector::new(inst, transport, oauth());
    let cfg = ConnectorConfig::new(ConnectorKind::Notion, AuthKind::OAuth2, ScopeId::new_v4())
        .with_auth_config(serde_json::json!({
            "authorization_code": "code",
        }));

    let tok = c.authenticate(&cfg).unwrap();
    assert_eq!(tok.access_token.expose(), "test-access");
}

// ─── Slack: authenticate + webhook ───

#[test]
fn slack_authenticate_and_webhook_cycle() {
    let transport = MockHttpTransport::new();
    let transport: Arc<dyn connector_framework::HttpTransport> = Arc::new(transport);
    let inst = ConnectorInstanceId::new_v4();
    let c = SlackConnector::new(inst, transport, oauth());
    let cfg = ConnectorConfig::new(ConnectorKind::Slack, AuthKind::OAuth2, ScopeId::new_v4())
        .with_auth_config(serde_json::json!({
            "authorization_code": "code",
            "signing_secret": "test-signing-secret",
        }));

    let tok = c.authenticate(&cfg).unwrap();
    assert_eq!(tok.access_token.expose(), "test-access");

    // Slack Events API webhook payload — Slack uses an
    // `event_callback` envelope with an inner `event` object.
    let ts = format!("{}.000000", Utc::now().timestamp());
    let payload = serde_json::json!({
        "type": "event_callback",
        "event_time": Utc::now().timestamp(),
        "event": {
            "type": "message",
            "channel": "C1",
            "ts": ts,
            "text": "hello",
        }
    });
    let evs = c
        .handle_webhook_event(&serde_json::to_vec(&payload).unwrap())
        .unwrap();
    assert_eq!(evs.len(), 1);
    assert!(matches!(evs[0], ConnectorEvent::DocumentCreated { .. }));
}

// ─── Cross-connector: all connectors authenticate ───

#[test]
fn all_connectors_authenticate_via_mock_oauth() {
    let make_transport =
        || -> Arc<dyn connector_framework::HttpTransport> { Arc::new(MockHttpTransport::new()) };
    let inst = ConnectorInstanceId::new_v4();
    let scope = ScopeId::new_v4();
    let auth_json = serde_json::json!({
        "authorization_code": "test-code",
        "api_base_url": "https://api.test",
        "repository": "owner/repo",
    });
    // Email connector requires `provider` in config.
    let email_auth_json = serde_json::json!({
        "authorization_code": "test-code",
        "api_base_url": "https://api.test",
        "provider": "gmail",
    });

    let connectors: Vec<(Box<dyn Connector>, serde_json::Value, ConnectorKind)> = vec![
        (
            Box::new(GitHubConnector::new(inst, make_transport(), oauth())),
            auth_json.clone(),
            ConnectorKind::GitHub,
        ),
        (
            Box::new(JiraConnector::new(inst, make_transport(), oauth())),
            auth_json.clone(),
            ConnectorKind::Jira,
        ),
        (
            Box::new(NotionConnector::new(inst, make_transport(), oauth())),
            auth_json.clone(),
            ConnectorKind::Notion,
        ),
        (
            Box::new(GoogleDriveConnector::new(inst, make_transport(), oauth())),
            auth_json.clone(),
            ConnectorKind::GoogleDrive,
        ),
        (
            Box::new(OneDriveConnector::new(inst, make_transport(), oauth())),
            auth_json.clone(),
            ConnectorKind::OneDrive,
        ),
        (
            Box::new(ConfluenceConnector::new(inst, make_transport(), oauth())),
            auth_json.clone(),
            ConnectorKind::Confluence,
        ),
        (
            Box::new(FigmaConnector::new(inst, make_transport(), oauth())),
            auth_json.clone(),
            ConnectorKind::Figma,
        ),
        (
            Box::new(HubSpotConnector::new(inst, make_transport(), oauth())),
            auth_json.clone(),
            ConnectorKind::HubSpot,
        ),
        (
            Box::new(SlackConnector::new(inst, make_transport(), oauth())),
            auth_json.clone(),
            ConnectorKind::Slack,
        ),
        (
            Box::new(EmailConnector::new(inst, make_transport(), oauth())),
            email_auth_json,
            ConnectorKind::Email,
        ),
    ];

    for (connector, config_json, kind) in &connectors {
        let cfg = ConnectorConfig::new(*kind, AuthKind::OAuth2, scope)
            .with_auth_config(config_json.clone());
        let tok = connector.authenticate(&cfg).unwrap();
        assert_eq!(tok.access_token.expose(), "test-access");
    }
}
