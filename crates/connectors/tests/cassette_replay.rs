//! Cassette replay integration tests for the exemplar connectors.
//!
//! Each test drives a connector's full source-facing lifecycle —
//! OAuth2 `authorization_code` exchange, `refresh_token` grant,
//! full `initial_sync`, `incremental_sync`, content fetch, webhook
//! parse, and ACL projection — entirely against a committed cassette
//! fixture under `tests/cassettes/<provider>/`. No network, no live
//! credentials: the [`ReplayTransport`] serves the recorded provider
//! responses back over the same [`HttpTransport`] boundary production
//! wires to `reqwest`, so the run is byte-for-byte deterministic in
//! CI.
//!
//! A connector graduates to `ConnectorMaturity::LiveVerified` only
//! once it has a test here; the five exemplars below
//! (github, slack, notion, momo, stripe) span developer tooling,
//! messaging, docs, a SEA e-wallet, and payments so the harness is
//! proven across the catalog's auth and pagination shapes.
//!
//! Re-recording: point a connector at a provider sandbox with a
//! `RecordingTransport` wrapping the reqwest-backed
//! `BlockingHttpTransport`, drive the same lifecycle once, and
//! `RecordingTransport::save` the scrubbed cassette. See
//! `docs/guides/add-a-connector.md`.

use std::sync::Arc;

use connector_framework::{AclSyncEngine, ReplayTransport};
use connector_framework::{
    Connector, ConnectorConfig, ConnectorEvent, ConnectorInstanceId, ConnectorKind,
    ConnectorMaturity, HttpTransport, OAuth2Client, OAuth2CodeExchange, OAuth2Token,
    PermissionDelta, PermissionMapping, SourceDocumentId, SourcePermission, SourcePermissionLevel,
    SourceRevocation, SourceUserId, SyncState,
};
use permission_service::{ObjectType, SubjectType, TupleStore};
use uuid::Uuid;

/// Build a replay transport from a committed cassette plus a real
/// [`OAuth2Client`] that shares the same transport, so OAuth2 grants
/// replay from the same fixture as the connector's data calls.
fn replay_clients(
    cassette_json: &str,
    client_secret: &str,
) -> (Arc<ReplayTransport>, Arc<OAuth2Client<ReplayTransport>>) {
    let replay = Arc::new(ReplayTransport::from_json(cassette_json).expect("load cassette"));
    let oauth = Arc::new(OAuth2Client::new(replay.clone()).with_client_secret(client_secret));
    (replay, oauth)
}

fn instance() -> ConnectorInstanceId {
    ConnectorInstanceId::new_v4()
}

/// Exercise the framework's ACL projection path with one grant and a
/// follow-up revocation, asserting the tuples land and clear in a
/// real [`TupleStore`]. Source ids are provider-shaped so the test
/// reads as the connector's own permission feed.
fn assert_acl_projection_round_trip(user: &str, document: &str) {
    let mut store = TupleStore::new();
    let mut mapping = PermissionMapping::new(ObjectType::Concept, SubjectType::User);
    let source_user = SourceUserId::new(user);
    let source_doc = SourceDocumentId::new(document);
    mapping.map_user(source_user.clone(), Uuid::new_v4());
    mapping.map_document(source_doc.clone(), Uuid::new_v4());

    let cid = instance();
    {
        let mut engine = AclSyncEngine::new(&mut store, &mapping);
        let grant = engine
            .sync(
                cid,
                &[PermissionDelta::Grant(SourcePermission::new(
                    source_user.clone(),
                    source_doc.clone(),
                    SourcePermissionLevel::Write,
                ))],
            )
            .expect("acl grant");
        assert_eq!(grant.inserted, 1, "grant must insert one tuple");
        assert!(grant.unknown_users.is_empty() && grant.unknown_documents.is_empty());
    }
    assert_eq!(store.len(), 1);

    {
        let mut engine = AclSyncEngine::new(&mut store, &mapping);
        let revoke = engine
            .sync(
                cid,
                &[PermissionDelta::Revoke(SourceRevocation {
                    source_user_id: source_user,
                    source_document_id: source_doc,
                })],
            )
            .expect("acl revoke");
        assert_eq!(revoke.removed, 1, "revoke must remove the tuple");
    }
    assert_eq!(store.len(), 0);
}

fn refresh_token_of(token: &OAuth2Token) -> String {
    token
        .refresh_token
        .as_ref()
        .expect("cassette token carries a refresh_token")
        .expose()
        .to_string()
}

// ───────────────────────── Stripe ─────────────────────────

mod stripe {
    use super::*;
    use connectors::stripe::StripeConnector;

    const CASSETTE: &str = include_str!("cassettes/stripe/lifecycle.json");
    const WEBHOOK: &[u8] = include_bytes!("cassettes/stripe/webhook_customer_created.json");
    const BASE_URL: &str = "https://api.test/stripe";

    fn config() -> ConnectorConfig {
        ConnectorConfig::new(
            ConnectorKind::Stripe,
            connector_framework::AuthKind::OAuth2,
            evidence_store::ScopeId::new_v4(),
        )
        .with_auth_config(serde_json::json!({
            "token_url": "https://connect.stripe.com/oauth/token",
            "client_id": "ca_test_client",
            "redirect_uri": "https://app.example.com/oauth/stripe",
            "authorization_code": "ac_demo",
            "api_base_url": BASE_URL,
        }))
    }

    #[test]
    fn full_lifecycle_replays_deterministically() {
        let (replay, oauth) = replay_clients(CASSETTE, "sk_client_secret");
        let connector = StripeConnector::new(
            instance(),
            replay.clone() as Arc<dyn HttpTransport>,
            oauth.clone() as Arc<dyn OAuth2CodeExchange>,
        );
        let cfg = config();

        // OAuth2 authorization_code exchange.
        let token = connector.authenticate(&cfg).expect("authenticate");
        assert_eq!(token.access_token.expose(), "sk_test_live_access");

        // OAuth2 refresh_token grant.
        let refreshed = oauth
            .refresh_with_config(&cfg, &refresh_token_of(&token))
            .expect("refresh");
        assert_eq!(refreshed.access_token.expose(), "sk_test_refreshed_access");

        // Full sync.
        let initial = connector.initial_sync(&cfg, &token).expect("initial_sync");
        assert_eq!(initial.events.len(), 1);
        assert!(matches!(
            initial.events[0],
            ConnectorEvent::DocumentCreated { .. }
        ));
        let cursor = initial.next_cursor.clone();
        assert!(cursor.is_some());

        // Incremental sync from the watermark cursor.
        let mut state = SyncState::new(connector.instance);
        state.cursor = cursor;
        let incremental = connector
            .incremental_sync(&cfg, &token, &state)
            .expect("incremental_sync");
        assert_eq!(incremental.events.len(), 1);
        assert!(matches!(
            incremental.events[0],
            ConnectorEvent::DocumentUpdated { .. }
        ));

        // Content fetch.
        let content = connector
            .fetch_content(&cfg, &token, &SourceDocumentId::new("cus_001"))
            .expect("fetch_content");
        assert!(String::from_utf8_lossy(&content.body).contains("Acme Pte Ltd"));

        // Webhook parse (offline).
        let events = connector.handle_webhook_event(WEBHOOK).expect("webhook");
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], ConnectorEvent::DocumentCreated { .. }));

        // Every recorded HTTP interaction was consumed exactly once.
        replay.assert_all_played();

        // ACL projection.
        assert_acl_projection_round_trip("acct_member_1", "cus_001");

        // Maturity is honestly surfaced.
        assert_eq!(
            ConnectorKind::Stripe.maturity(),
            ConnectorMaturity::LiveVerified
        );
    }
}

// ───────────────────────── Notion ─────────────────────────

mod notion {
    use super::*;
    use connectors::notion::NotionConnector;

    const CASSETTE: &str = include_str!("cassettes/notion/lifecycle.json");
    const BASE_URL: &str = "https://api.test/notion";

    fn config() -> ConnectorConfig {
        ConnectorConfig::new(
            ConnectorKind::Notion,
            connector_framework::AuthKind::OAuth2,
            evidence_store::ScopeId::new_v4(),
        )
        .with_auth_config(serde_json::json!({
            "token_url": "https://api.notion.com/v1/oauth/token",
            "client_id": "notion_client",
            "redirect_uri": "https://app.example.com/oauth/notion",
            "authorization_code": "ac_demo",
            "api_base_url": BASE_URL,
        }))
    }

    #[test]
    fn full_lifecycle_replays_deterministically() {
        let (replay, oauth) = replay_clients(CASSETTE, "notion_client_secret");
        let connector = NotionConnector::new(
            instance(),
            replay.clone() as Arc<dyn HttpTransport>,
            oauth.clone() as Arc<dyn OAuth2CodeExchange>,
        );
        let cfg = config();

        let token = connector.authenticate(&cfg).expect("authenticate");
        assert_eq!(token.access_token.expose(), "notion_access");

        let refreshed = oauth
            .refresh_with_config(&cfg, &refresh_token_of(&token))
            .expect("refresh");
        assert_eq!(refreshed.access_token.expose(), "notion_refreshed");

        let initial = connector.initial_sync(&cfg, &token).expect("initial_sync");
        assert_eq!(initial.events.len(), 1);
        assert!(matches!(
            initial.events[0],
            ConnectorEvent::DocumentCreated { .. }
        ));

        let mut state = SyncState::new(connector.instance);
        state.cursor = initial.next_cursor.clone();
        let incremental = connector
            .incremental_sync(&cfg, &token, &state)
            .expect("incremental_sync");
        assert_eq!(incremental.events.len(), 1);
        assert!(matches!(
            incremental.events[0],
            ConnectorEvent::DocumentUpdated { .. }
        ));

        let content = connector
            .fetch_content(&cfg, &token, &SourceDocumentId::new("page-1"))
            .expect("fetch_content");
        assert!(String::from_utf8_lossy(&content.body).contains("Hello from the Acme runbook."));
        assert_eq!(content.title.as_deref(), Some("Acme Runbook"));

        // Notion is polling-only: both webhook surfaces must report so.
        assert!(connector
            .subscribe_webhook(&cfg, &token, "https://hooks.example.com/notion")
            .is_err());
        assert!(connector.handle_webhook_event(b"{}").is_err());

        replay.assert_all_played();
        assert_acl_projection_round_trip("notion_user_1", "page-1");
        assert_eq!(
            ConnectorKind::Notion.maturity(),
            ConnectorMaturity::LiveVerified
        );
    }
}

// ───────────────────────── MoMo ─────────────────────────

mod momo {
    use super::*;
    use connectors::momo::MoMoConnector;

    const CASSETTE: &str = include_str!("cassettes/momo/lifecycle.json");
    const WEBHOOK: &[u8] = include_bytes!("cassettes/momo/webhook_ipn.json");
    const BASE_URL: &str = "https://api.test/momo";

    fn config() -> ConnectorConfig {
        ConnectorConfig::new(
            ConnectorKind::MoMo,
            connector_framework::AuthKind::OAuth2,
            evidence_store::ScopeId::new_v4(),
        )
        .with_auth_config(serde_json::json!({
            "token_url": "https://business.momo.vn/oauth/token",
            "client_id": "momo_client",
            "redirect_uri": "https://app.example.com/oauth/momo",
            "authorization_code": "ac_demo",
            "api_base_url": BASE_URL,
            "ipn_secret": "momo_ipn_secret",
        }))
    }

    #[test]
    fn full_lifecycle_replays_deterministically() {
        let (replay, oauth) = replay_clients(CASSETTE, "momo_client_secret");
        let connector = MoMoConnector::new(
            instance(),
            replay.clone() as Arc<dyn HttpTransport>,
            oauth.clone() as Arc<dyn OAuth2CodeExchange>,
        );
        let cfg = config();

        let token = connector.authenticate(&cfg).expect("authenticate");
        assert_eq!(token.access_token.expose(), "momo_access");

        let refreshed = oauth
            .refresh_with_config(&cfg, &refresh_token_of(&token))
            .expect("refresh");
        assert_eq!(refreshed.access_token.expose(), "momo_refreshed");

        let initial = connector.initial_sync(&cfg, &token).expect("initial_sync");
        assert_eq!(initial.events.len(), 1);
        assert!(matches!(
            initial.events[0],
            ConnectorEvent::DocumentCreated { .. }
        ));

        let mut state = SyncState::new(connector.instance);
        state.cursor = initial.next_cursor.clone();
        let incremental = connector
            .incremental_sync(&cfg, &token, &state)
            .expect("incremental_sync");
        assert_eq!(incremental.events.len(), 1);
        assert!(matches!(
            incremental.events[0],
            ConnectorEvent::DocumentUpdated { .. }
        ));

        let content = connector
            .fetch_content(&cfg, &token, &SourceDocumentId::new("ORD-1"))
            .expect("fetch_content");
        assert!(String::from_utf8_lossy(&content.body).contains("Transaction ORD-1"));

        // IPN webhook secret is captured from config (no HTTP call).
        let sub = connector
            .subscribe_webhook(&cfg, &token, "https://hooks.example.com/momo")
            .expect("subscribe_webhook");
        assert_eq!(sub.connector, connector.instance);

        let events = connector.handle_webhook_event(WEBHOOK).expect("webhook");
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], ConnectorEvent::DocumentUpdated { .. }));

        replay.assert_all_played();
        assert_acl_projection_round_trip("momo_merchant_1", "ORD-1");
        assert_eq!(
            ConnectorKind::MoMo.maturity(),
            ConnectorMaturity::LiveVerified
        );
    }
}

// ───────────────────────── Slack ─────────────────────────

mod slack {
    use super::*;
    use connectors::slack::SlackConnector;

    const CASSETTE: &str = include_str!("cassettes/slack/lifecycle.json");
    const WEBHOOK: &[u8] = include_bytes!("cassettes/slack/webhook_message.json");
    const BASE_URL: &str = "https://api.test/slack";

    fn config() -> ConnectorConfig {
        ConnectorConfig::new(
            ConnectorKind::Slack,
            connector_framework::AuthKind::OAuth2,
            evidence_store::ScopeId::new_v4(),
        )
        .with_auth_config(serde_json::json!({
            "token_url": "https://slack.com/api/oauth.v2.access",
            "client_id": "slack_client",
            "redirect_uri": "https://app.example.com/oauth/slack",
            "authorization_code": "ac_demo",
            "api_base_url": BASE_URL,
            "signing_secret": "slack_signing_secret",
        }))
    }

    #[test]
    fn full_lifecycle_replays_deterministically() {
        let (replay, oauth) = replay_clients(CASSETTE, "slack_client_secret");
        let connector = SlackConnector::new(
            instance(),
            replay.clone() as Arc<dyn HttpTransport>,
            oauth.clone() as Arc<dyn OAuth2CodeExchange>,
        );
        let cfg = config();

        let token = connector.authenticate(&cfg).expect("authenticate");
        assert_eq!(token.access_token.expose(), "xoxb-slack-access");

        let refreshed = oauth
            .refresh_with_config(&cfg, &refresh_token_of(&token))
            .expect("refresh");
        assert_eq!(refreshed.access_token.expose(), "xoxb-slack-refreshed");

        let initial = connector.initial_sync(&cfg, &token).expect("initial_sync");
        assert_eq!(initial.events.len(), 1);
        assert!(matches!(
            initial.events[0],
            ConnectorEvent::DocumentCreated { .. }
        ));

        let mut state = SyncState::new(connector.instance);
        state.cursor = initial.next_cursor.clone();
        let incremental = connector
            .incremental_sync(&cfg, &token, &state)
            .expect("incremental_sync");
        assert_eq!(incremental.events.len(), 1);
        assert!(matches!(
            incremental.events[0],
            ConnectorEvent::DocumentUpdated { .. }
        ));

        let content = connector
            .fetch_content(
                &cfg,
                &token,
                &SourceDocumentId::new("slack:C1:1700000000.000100"),
            )
            .expect("fetch_content");
        assert!(String::from_utf8_lossy(&content.body).contains("Welcome to Acme support"));

        let sub = connector
            .subscribe_webhook(&cfg, &token, "https://hooks.example.com/slack")
            .expect("subscribe_webhook");
        assert_eq!(sub.connector, connector.instance);

        let events = connector.handle_webhook_event(WEBHOOK).expect("webhook");
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], ConnectorEvent::DocumentCreated { .. }));

        replay.assert_all_played();
        assert_acl_projection_round_trip("slack_user_1", "slack:C1:1700000000.000100");
        assert_eq!(
            ConnectorKind::Slack.maturity(),
            ConnectorMaturity::LiveVerified
        );
    }
}

// ───────────────────────── GitHub ─────────────────────────

mod github {
    use super::*;
    use connectors::github::GitHubConnector;

    const CASSETTE: &str = include_str!("cassettes/github/lifecycle.json");
    const WEBHOOK: &[u8] = include_bytes!("cassettes/github/webhook_issue_opened.json");
    const BASE_URL: &str = "https://api.test/github";

    fn config() -> ConnectorConfig {
        ConnectorConfig::new(
            ConnectorKind::GitHub,
            connector_framework::AuthKind::OAuth2,
            evidence_store::ScopeId::new_v4(),
        )
        .with_auth_config(serde_json::json!({
            "token_url": "https://github.com/login/oauth/access_token",
            "client_id": "github_client",
            "redirect_uri": "https://app.example.com/oauth/github",
            "authorization_code": "ac_demo",
            "api_base_url": BASE_URL,
            "repository": "owner/acme",
            "webhook_secret": "github_webhook_secret",
        }))
    }

    #[test]
    fn full_lifecycle_replays_deterministically() {
        let (replay, oauth) = replay_clients(CASSETTE, "github_client_secret");
        let connector = GitHubConnector::new(
            instance(),
            replay.clone() as Arc<dyn HttpTransport>,
            oauth.clone() as Arc<dyn OAuth2CodeExchange>,
        );
        let cfg = config();

        let token = connector.authenticate(&cfg).expect("authenticate");
        assert_eq!(token.access_token.expose(), "gho_github_access");

        let refreshed = oauth
            .refresh_with_config(&cfg, &refresh_token_of(&token))
            .expect("refresh");
        assert_eq!(refreshed.access_token.expose(), "gho_github_refreshed");

        let initial = connector.initial_sync(&cfg, &token).expect("initial_sync");
        assert_eq!(initial.events.len(), 1);
        assert!(matches!(
            initial.events[0],
            ConnectorEvent::DocumentCreated { .. }
        ));

        let mut state = SyncState::new(connector.instance);
        state.cursor = initial.next_cursor.clone();
        let incremental = connector
            .incremental_sync(&cfg, &token, &state)
            .expect("incremental_sync");
        assert_eq!(incremental.events.len(), 1);
        assert!(matches!(
            incremental.events[0],
            ConnectorEvent::DocumentUpdated { .. }
        ));

        let content = connector
            .fetch_content(&cfg, &token, &SourceDocumentId::new("101"))
            .expect("fetch_content");
        let body = String::from_utf8_lossy(&content.body);
        assert!(body.contains("Login fails on Safari"));
        assert!(body.contains("I can reproduce this too."));

        let sub = connector
            .subscribe_webhook(&cfg, &token, "https://hooks.example.com/github")
            .expect("subscribe_webhook");
        assert_eq!(sub.provider_subscription_id.as_deref(), Some("12345678"));

        let events = connector.handle_webhook_event(WEBHOOK).expect("webhook");
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], ConnectorEvent::DocumentCreated { .. }));

        replay.assert_all_played();
        assert_acl_projection_round_trip("github_user_1", "101");
        assert_eq!(
            ConnectorKind::GitHub.maturity(),
            ConnectorMaturity::LiveVerified
        );
    }
}
