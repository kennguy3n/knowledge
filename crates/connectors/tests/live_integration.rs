//! Live-integration smoke tests for the 9 production connectors.
//!
//! Every other test in this crate uses
//! [`connector_framework::http::MockHttpTransport`] (or the
//! framework's `test-support` shim) and runs against canned JSON.
//! That gives us deterministic coverage of the request shaping and
//! response decoding, but it does **not** prove the connector can
//! talk to the real provider — header casing, OAuth scopes,
//! pagination parameter names, and rate-limit headers all only
//! show up against live traffic.
//!
//! This module sits behind two layers of gating so it never
//! accidentally runs in default CI:
//!
//! 1. **Cargo feature.** The harness only compiles when
//!    `--features live-integration` is on (see `Cargo.toml`). That
//!    feature transitively enables `http-client`, which in turn
//!    pulls reqwest into the framework's `BlockingHttpTransport`.
//!    Without the feature, the file compiles to an empty module
//!    so `cargo test --workspace` keeps working.
//! 2. **Per-provider env vars.** Each test reads its bearer token
//!    from a uniquely-named env var (e.g. `NOTION_TEST_TOKEN`).
//!    If the var is unset, the test logs a skip and returns —
//!    matching the
//!    `crates/inference_router::tests::llama_cpp` skip-on-missing-
//!    `LLAMA_SERVER_BINARY` pattern. A developer running
//!    `cargo test --features live-integration` against a sandbox
//!    account can opt into one provider at a time without
//!    leaking credentials into the test matrix.
//!
//! ## Token shape
//!
//! Every test treats the env var as an already-issued bearer
//! token and constructs [`OAuth2Token::new_without_refresh`]
//! directly. This sidesteps the OAuth2 `authorization_code`
//! exchange entirely — that flow needs a user browser hop and
//! cannot run unattended in CI. Developers running the harness
//! against a sandbox account should grant the token whatever
//! read-only scopes the connector advertises (e.g. Notion's
//! `read_content`, Slack's `channels:read`, Drive's
//! `drive.metadata.readonly`).
//!
//! ## Provider env var matrix
//!
//! | Connector     | Token env var               | Optional config env var(s)              |
//! | ------------- | --------------------------- | --------------------------------------- |
//! | Notion        | `NOTION_TEST_TOKEN`         | —                                       |
//! | Google Drive  | `GOOGLE_DRIVE_TEST_TOKEN`   | —                                       |
//! | Slack         | `SLACK_TEST_TOKEN`          | —                                       |
//! | Jira          | `JIRA_TEST_TOKEN`           | `JIRA_TEST_BASE_URL`                    |
//! | Confluence    | `CONFLUENCE_TEST_TOKEN`     | `CONFLUENCE_TEST_BASE_URL`              |
//! | OneDrive      | `ONEDRIVE_TEST_TOKEN`       | —                                       |
//! | HubSpot       | `HUBSPOT_TEST_TOKEN`        | —                                       |
//! | Figma         | `FIGMA_TEST_TOKEN`          | —                                       |
//! | Email (Gmail) | `EMAIL_TEST_TOKEN`          | `EMAIL_TEST_PROVIDER` (gmail \| graph)  |
//!
//! Tests assert `initial_sync` returns at least one
//! [`ConnectorEvent`] — a connected sandbox account is expected
//! to have at least one document / page / message to surface.

#![cfg(feature = "live-integration")]

use std::sync::Arc;

use chrono::{Duration, Utc};
use connector_framework::config::{AuthKind, ConnectorConfig, ConnectorKind};
use connector_framework::connector::Connector;
use connector_framework::http::BlockingHttpTransport;
use connector_framework::oauth::default_oauth_client;
use connector_framework::token_vault::{ConnectorInstanceId, OAuth2Token};
use evidence_store::ScopeId;
use uuid::Uuid;

use connectors::confluence::ConfluenceConnector;
use connectors::email::EmailConnector;
use connectors::figma::FigmaConnector;
use connectors::google_drive::GoogleDriveConnector;
use connectors::hubspot::HubSpotConnector;
use connectors::jira::JiraConnector;
use connectors::notion::NotionConnector;
use connectors::onedrive::OneDriveConnector;
use connectors::slack::SlackConnector;

/// Read a required env var or skip the test.
///
/// Returns `Some(value)` when the var is set; otherwise prints a
/// skip line (visible under `cargo test -- --nocapture`) and
/// returns `None` so the caller can `return` from the test body.
fn env_or_skip(var: &str, label: &str) -> Option<String> {
    match std::env::var(var) {
        Ok(v) if !v.is_empty() => Some(v),
        _ => {
            eprintln!("[live_integration] skip {label}: env var {var} not set");
            None
        }
    }
}

/// Wire a reqwest-backed [`BlockingHttpTransport`] +
/// `OAuth2Client` pair against the framework's default OAuth
/// client builder.
fn live_clients() -> (Arc<BlockingHttpTransport>,
    Arc<connector_framework::oauth::OAuth2Client<BlockingHttpTransport>>,
) {
    // Both fail-paths here mean the test environment itself is
    // broken (no reqwest TLS, etc.) — propagate as a panic so the
    // developer running the harness sees the real error rather
    // than a silent skip.
    let transport = Arc::new(BlockingHttpTransport::new().expect("BlockingHttpTransport must initialise for live tests"),
    );
    let oauth = Arc::new(default_oauth_client().expect("default_oauth_client must initialise for live tests"),
    );
    (transport, oauth)
}

/// Wrap an already-issued bearer token in an [`OAuth2Token`].
///
/// The expiry is set 1h into the future — every test runs in well
/// under a second so the value only matters for the framework's
/// `is_expired` guard.
fn bearer_token(access: &str, scope: &str) -> OAuth2Token {
    OAuth2Token::new_without_refresh(access, Utc::now() + Duration::hours(1), scope)
}

/// Build a connector config for one of the well-known providers.
fn config_for(kind: ConnectorKind, auth: AuthKind) -> ConnectorConfig {
    ConnectorConfig::new(kind, auth, ScopeId::from_uuid(Uuid::new_v4()))
}

#[test]
fn notion_initial_sync_returns_at_least_one_event() {
    let Some(token) = env_or_skip("NOTION_TEST_TOKEN", "notion") else {
        return;
    };
    let (transport, oauth) = live_clients();
    let connector = NotionConnector::new(ConnectorInstanceId(Uuid::new_v4()), transport, oauth);
    let cfg = config_for(ConnectorKind::Notion, AuthKind::OAuth2);
    let bearer = bearer_token(&token, "read_content");

    let result = connector
        .initial_sync(&cfg, &bearer)
        .expect("notion initial_sync against the sandbox account must succeed");
    assert!(!result.events.is_empty(),
        "notion sandbox account must surface at least one page/database"
    );
}

#[test]
fn google_drive_initial_sync_returns_at_least_one_event() {
    let Some(token) = env_or_skip("GOOGLE_DRIVE_TEST_TOKEN", "google_drive") else {
        return;
    };
    let (transport, oauth) = live_clients();
    let connector =
        GoogleDriveConnector::new(ConnectorInstanceId(Uuid::new_v4()), transport, oauth);
    let cfg = config_for(ConnectorKind::GoogleDrive, AuthKind::OAuth2);
    let bearer = bearer_token(&token,
        "https://www.googleapis.com/auth/drive.metadata.readonly",
    );

    let result = connector
        .initial_sync(&cfg, &bearer)
        .expect("google drive initial_sync against the sandbox account must succeed");
    assert!(!result.events.is_empty(),
        "google drive sandbox account must surface at least one file"
    );
}

#[test]
fn slack_initial_sync_returns_at_least_one_event() {
    let Some(token) = env_or_skip("SLACK_TEST_TOKEN", "slack") else {
        return;
    };
    let (transport, oauth) = live_clients();
    let connector = SlackConnector::new(ConnectorInstanceId(Uuid::new_v4()), transport, oauth);
    let cfg = config_for(ConnectorKind::Slack, AuthKind::OAuth2);
    let bearer = bearer_token(&token, "channels:read,channels:history");

    let result = connector
        .initial_sync(&cfg, &bearer)
        .expect("slack initial_sync against the sandbox workspace must succeed");
    assert!(!result.events.is_empty(),
        "slack sandbox workspace must surface at least one channel"
    );
}

#[test]
fn jira_initial_sync_returns_at_least_one_event() {
    let Some(token) = env_or_skip("JIRA_TEST_TOKEN", "jira") else {
        return;
    };
    let base_url = std::env::var("JIRA_TEST_BASE_URL").unwrap_or_else(|_| {
        panic!("JIRA_TEST_TOKEN is set but JIRA_TEST_BASE_URL is missing (e.g. https://acme.atlassian.net)")
    });
    let (transport, oauth) = live_clients();
    let connector = JiraConnector::new(ConnectorInstanceId(Uuid::new_v4()), transport, oauth)
        .with_api_base_url(base_url);
    let cfg = config_for(ConnectorKind::Jira, AuthKind::OAuth2);
    let bearer = bearer_token(&token, "read:jira-work");

    let result = connector
        .initial_sync(&cfg, &bearer)
        .expect("jira initial_sync against the sandbox project must succeed");
    assert!(!result.events.is_empty(),
        "jira sandbox project must surface at least one issue"
    );
}

#[test]
fn confluence_initial_sync_returns_at_least_one_event() {
    let Some(token) = env_or_skip("CONFLUENCE_TEST_TOKEN", "confluence") else {
        return;
    };
    let base_url = std::env::var("CONFLUENCE_TEST_BASE_URL").unwrap_or_else(|_| {
        panic!("CONFLUENCE_TEST_TOKEN is set but CONFLUENCE_TEST_BASE_URL is missing (e.g. https://acme.atlassian.net)")
    });
    let (transport, oauth) = live_clients();
    let connector = ConfluenceConnector::new(ConnectorInstanceId(Uuid::new_v4()), transport, oauth)
        .with_api_base_url(base_url);
    let cfg = config_for(ConnectorKind::Confluence, AuthKind::OAuth2);
    let bearer = bearer_token(&token, "read:confluence-content.summary");

    let result = connector
        .initial_sync(&cfg, &bearer)
        .expect("confluence initial_sync against the sandbox space must succeed");
    assert!(!result.events.is_empty(),
        "confluence sandbox space must surface at least one page"
    );
}

#[test]
fn onedrive_initial_sync_returns_at_least_one_event() {
    let Some(token) = env_or_skip("ONEDRIVE_TEST_TOKEN", "onedrive") else {
        return;
    };
    let (transport, oauth) = live_clients();
    let connector = OneDriveConnector::new(ConnectorInstanceId(Uuid::new_v4()), transport, oauth);
    let cfg = config_for(ConnectorKind::OneDrive, AuthKind::OAuth2);
    let bearer = bearer_token(&token, "Files.Read");

    let result = connector
        .initial_sync(&cfg, &bearer)
        .expect("onedrive initial_sync against the sandbox account must succeed");
    assert!(!result.events.is_empty(),
        "onedrive sandbox account must surface at least one file"
    );
}

#[test]
fn hubspot_initial_sync_returns_at_least_one_event() {
    let Some(token) = env_or_skip("HUBSPOT_TEST_TOKEN", "hubspot") else {
        return;
    };
    let (transport, oauth) = live_clients();
    let connector = HubSpotConnector::new(ConnectorInstanceId(Uuid::new_v4()), transport, oauth);
    let cfg = config_for(ConnectorKind::HubSpot, AuthKind::OAuth2);
    let bearer = bearer_token(&token, "crm.objects.contacts.read");

    let result = connector
        .initial_sync(&cfg, &bearer)
        .expect("hubspot initial_sync against the sandbox portal must succeed");
    assert!(!result.events.is_empty(),
        "hubspot sandbox portal must surface at least one contact/company/deal"
    );
}

#[test]
fn figma_initial_sync_returns_at_least_one_event() {
    let Some(token) = env_or_skip("FIGMA_TEST_TOKEN", "figma") else {
        return;
    };
    let (transport, oauth) = live_clients();
    let connector = FigmaConnector::new(ConnectorInstanceId(Uuid::new_v4()), transport, oauth);
    let cfg = config_for(ConnectorKind::Figma, AuthKind::OAuth2);
    let bearer = bearer_token(&token, "file_read");

    let result = connector
        .initial_sync(&cfg, &bearer)
        .expect("figma initial_sync against the sandbox team must succeed");
    assert!(!result.events.is_empty(),
        "figma sandbox team must surface at least one file"
    );
}

#[test]
fn email_initial_sync_returns_at_least_one_event() {
    let Some(token) = env_or_skip("EMAIL_TEST_TOKEN", "email") else {
        return;
    };
    let (transport, oauth) = live_clients();
    let connector = EmailConnector::new(ConnectorInstanceId(Uuid::new_v4()), transport, oauth);
    // Email connector chooses Gmail vs Graph based on
    // auth_config_json["provider"]; the harness mirrors that
    // single env var through into the connector config so the
    // developer running the test does not have to know the JSON
    // shape.
    let provider = std::env::var("EMAIL_TEST_PROVIDER").unwrap_or_else(|_| "gmail".to_string());
    let mut cfg = config_for(ConnectorKind::Email, AuthKind::OAuth2);
    cfg.auth_config_json = serde_json::json!({ "provider": provider });
    let bearer = bearer_token(&token, "https://www.googleapis.com/auth/gmail.readonly");

    let result = connector
        .initial_sync(&cfg, &bearer)
        .expect("email initial_sync against the sandbox inbox must succeed");
    assert!(!result.events.is_empty(),
        "email sandbox inbox must surface at least one message"
    );
}
