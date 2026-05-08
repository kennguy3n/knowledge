//! Integration tests for the HubSpot connector — exercises the full
//! sync → incremental → webhook cycle against fixture JSON modelled on
//! HubSpot CRM API v3 and HubSpot webhook subscription payloads.

use connector_framework::{
    AuthKind, Connector, ConnectorConfig, ConnectorError, ConnectorEvent, ConnectorInstanceId,
    ConnectorKind, SyncState,
};
use connectors::hubspot::{HubSpotConnector, HubSpotListResponse};
use evidence_store::ScopeId;

const INITIAL_FIXTURE: &str = include_str!("fixtures/hubspot_initial.json");
const INCREMENTAL_FIXTURE: &str = include_str!("fixtures/hubspot_incremental.json");
const WEBHOOK_FIXTURE: &str = include_str!("fixtures/hubspot_webhook.json");

fn cfg() -> ConnectorConfig {
    ConnectorConfig::new(ConnectorKind::HubSpot, AuthKind::OAuth2, ScopeId::new_v4())
}

fn build_connector() -> HubSpotConnector {
    let initial: HubSpotListResponse =
        serde_json::from_str(INITIAL_FIXTURE).expect("parse initial fixture");
    let incremental: HubSpotListResponse =
        serde_json::from_str(INCREMENTAL_FIXTURE).expect("parse incremental fixture");
    HubSpotConnector::new(ConnectorInstanceId::new_v4())
        .with_initial_pages(vec![initial])
        .with_incremental_pages(vec![incremental])
}

#[test]
fn full_lifecycle_against_fixture_data() {
    let connector = build_connector();
    let config = cfg();

    let token = connector.authenticate(&config).expect("authenticate");
    assert!(!token.access_token.expose().is_empty());
    assert!(token.scope.contains("crm"));

    let initial = connector
        .initial_sync(&config, &token)
        .expect("initial_sync");
    assert_eq!(initial.events.len(), 2, "two CRM objects in fixture");
    for ev in &initial.events {
        assert!(matches!(ev, ConnectorEvent::DocumentCreated { .. }));
    }
    assert!(initial.next_cursor.is_some(), "watermark cursor seeded");

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
    assert_eq!(deleted, 1, "archived deal maps to delete");

    let sub = connector
        .subscribe_webhook(&config, &token, "https://substrate.example/hubspot")
        .expect("subscribe_webhook");
    assert_eq!(sub.connector, connector.instance);

    let event = connector
        .handle_webhook_event(WEBHOOK_FIXTURE.as_bytes())
        .expect("handle_webhook_event");
    match event {
        ConnectorEvent::DocumentCreated { document_id, .. } => {
            assert_eq!(
                document_id.as_str(),
                "contact:5050",
                "HubSpot encodes object kind in the document id",
            );
        }
        other => panic!("expected DocumentCreated, got {other:?}"),
    }
}

#[test]
fn invalid_json_webhook_returns_error() {
    let connector = build_connector();
    let err = connector.handle_webhook_event(b"<not json>").unwrap_err();
    assert!(matches!(err, ConnectorError::Json(_)));
}

#[test]
fn empty_webhook_batch_returns_webhook_error() {
    let connector = build_connector();
    let body = serde_json::json!([]);
    let err = connector
        .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
        .unwrap_err();
    assert!(matches!(err, ConnectorError::Webhook(_)));
}

#[test]
fn unknown_subscription_type_returns_webhook_error() {
    let connector = build_connector();
    let body = serde_json::json!([
        {
            "subscriptionType": "weird.event.type",
            "objectId": 1,
            "occurredAt": 1746374700000_i64,
        }
    ]);
    let err = connector
        .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
        .unwrap_err();
    assert!(matches!(err, ConnectorError::Webhook(_)));
}
