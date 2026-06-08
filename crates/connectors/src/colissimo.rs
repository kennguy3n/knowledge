//! Colissimo connector — La Poste *Suivi v2* tracking API
//! (`https://api.laposte.fr/suivi/v2`).
//!
//! Colissimo parcels are tracked through La Poste's "Suivi v2" Okapi
//! API, which harmonises tracking for Courrier Suivi, Colissimo and
//! Chronopost. The legacy `ws.colissimo.fr` host only exposes the
//! SOAP/REST *label-generation* web service (`/sls-ws/...`) and has no
//! parcel-listing endpoint (a `GET /v1/parcels` there returns 404), so
//! this connector reads tracking data from Suivi v2 instead.
//!
//! Authentication uses La Poste's Okapi gateway key: a static key is
//! presented in the provider-native `X-Okapi-Key` header (read from
//! `auth_config_json.api_key`), falling back to the injected
//! [`OAuth2CodeExchange`] when a rotating `authorization_code` grant is
//! configured instead. The request auth header is chosen from the
//! token's provenance (recorded in [`OAuth2Token::token_type`]).
//!
//! Suivi v2 has no enumeration endpoint — tracking is keyed by parcel
//! number (`idShip`) — so the set of parcels to follow is named by
//! `auth_config_json.idships` (comma-separated; fail-fast if missing).
//!
//! * `initial_sync` GETs `/idships/{idship}` for each configured parcel
//!   and emits `DocumentCreated`, tracking the latest tracking-event
//!   timestamp as an RFC-3339 watermark.
//! * `incremental_sync` re-fetches each parcel and emits
//!   `DocumentUpdated` only when its latest event advanced past the
//!   stored watermark.
//! * `fetch_content` GETs a single parcel (`/idships/{idship}`) and
//!   renders its product and tracking history.
//! * Webhooks are configured in the provider dashboard, so
//!   `subscribe_webhook` records a polling-only subscription.
//! * `handle_webhook_event` parses the delivered payload.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use connector_framework::{
    apply_auth_by_provenance, classify_failure, percent_encode_path_component, Connector,
    ConnectorConfig, ConnectorError, ConnectorEvent, ConnectorInstanceId, FetchedContent,
    HttpRequest, HttpTransport, OAuth2CodeExchange, OAuth2Token, Result, SourceDocumentId,
    SyncRunResult, SyncState, WebhookEventTypes, WebhookSecret, WebhookSubscription,
};
use serde::{Deserialize, Serialize};

/// Default Colissimo tracking API base URL (La Poste Suivi v2).
pub const DEFAULT_API_BASE_URL: &str = "https://api.laposte.fr/suivi/v2";
/// Default scope recorded on the synthesised API-key token.
pub const DEFAULT_SCOPE: &str = "suivi";
/// `OAuth2Token::token_type` marker for a static API-key credential.
/// Distinguishes the API-key auth path (provider-native `X-Okapi-Key`
/// header) from an OAuth-issued bearer token.
pub const API_KEY_TOKEN_TYPE: &str = "ApiKey";
/// Safety ceiling on the number of parcels a single sync walks.
pub const MAX_PARCELS: usize = 100_000;

/// La Poste Suivi v2 single-parcel response envelope.
#[derive(Debug, Clone, Default, Deserialize)]
struct ColissimoResponse {
    #[serde(default, rename = "returnCode")]
    return_code: Option<i64>,
    #[serde(default)]
    shipment: Option<ColissimoShipment>,
}

/// A tracked shipment. La Poste uses camelCase field names.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ColissimoShipment {
    #[serde(default, rename = "idShip")]
    id_ship: String,
    #[serde(default)]
    product: Option<String>,
    #[serde(default, rename = "isFinal")]
    is_final: Option<bool>,
    #[serde(default, rename = "entryDate")]
    entry_date: Option<String>,
    #[serde(default)]
    event: Vec<ColissimoEvent>,
    #[serde(default)]
    url: Option<String>,
}

/// A single tracking event in the shipment history.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ColissimoEvent {
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    date: Option<String>,
}

impl ColissimoShipment {
    /// The most recent tracking-event timestamp, used as the per-parcel
    /// change watermark. Falls back to the shipment entry date.
    fn latest_event_at(&self) -> Option<DateTime<Utc>> {
        self.event
            .iter()
            .filter_map(|e| e.date.as_deref().and_then(parse_rfc3339))
            .max()
            .or_else(|| self.entry_date.as_deref().and_then(parse_rfc3339))
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ColissimoWebhookEvent {
    #[serde(default, alias = "idShip", alias = "parcel_id", alias = "id")]
    id_ship: serde_json::Value,
    #[serde(default, alias = "eventType")]
    event: String,
}

/// Colissimo connector.
pub struct ColissimoConnector {
    /// Connector instance id.
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    oauth: Arc<dyn OAuth2CodeExchange>,
    api_base_url: String,
}

impl std::fmt::Debug for ColissimoConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ColissimoConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("transport", &"<HttpTransport>")
            .field("oauth", &"<OAuth2CodeExchange>")
            .finish()
    }
}

impl ColissimoConnector {
    /// Construct a Colissimo connector.
    pub fn new(
        instance: ConnectorInstanceId,
        transport: Arc<dyn HttpTransport>,
        oauth: Arc<dyn OAuth2CodeExchange>,
    ) -> Self {
        Self {
            instance,
            transport,
            oauth,
            api_base_url: DEFAULT_API_BASE_URL.to_string(),
        }
    }

    /// Override the Colissimo API base URL.
    #[must_use]
    pub fn with_api_base_url(mut self, url: impl Into<String>) -> Self {
        self.api_base_url = url.into();
        self
    }

    fn resolved_base_url(&self, config: &ConnectorConfig) -> String {
        config
            .auth_config_json
            .get("api_base_url")
            .and_then(serde_json::Value::as_str)
            .map_or_else(
                || self.api_base_url.clone(),
                std::string::ToString::to_string,
            )
    }

    /// The parcel numbers to track. Suivi v2 has no enumeration, so this
    /// is required; fail fast with a clear message if it is missing.
    fn configured_idships(config: &ConnectorConfig) -> Result<Vec<String>> {
        let raw = config
            .auth_config_json
            .get("idships")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ConnectorError::Sync(
                    "colissimo sync requires auth_config_json.idships (comma-separated parcel numbers)"
                        .into(),
                )
            })?;
        let idships: Vec<String> = raw
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(std::string::ToString::to_string)
            .collect();
        if idships.is_empty() {
            return Err(ConnectorError::Sync(
                "colissimo auth_config_json.idships is empty".into(),
            ));
        }
        if idships.len() > MAX_PARCELS {
            return Err(ConnectorError::Sync(format!(
                "colissimo idships count {} exceeds {MAX_PARCELS}",
                idships.len()
            )));
        }
        Ok(idships)
    }

    /// Fetch a single parcel's tracking, returning `None` when La Poste
    /// has no record for that parcel number (so one unknown idship does
    /// not abort the whole sync).
    fn fetch_shipment(
        &self,
        base_url: &str,
        idship: &str,
        token: &OAuth2Token,
    ) -> Result<Option<ColissimoShipment>> {
        let id_enc = percent_encode_path_component(idship);
        let url = format!("{base_url}/idships/{id_enc}");
        let req = apply_auth(
            HttpRequest::get(&url).with_header("Accept", "application/json"),
            token,
        );
        let resp = self.transport.execute(req)?;
        // La Poste returns 404 for an unknown parcel number; treat that
        // as "no record yet" rather than a fatal error.
        if resp.status == 404 {
            return Ok(None);
        }
        if !resp.is_success() {
            return Err(classify_failure("colissimo", "/idships/{idship}", &resp));
        }
        let parsed: ColissimoResponse = serde_json::from_slice(&resp.body).map_err(|e| {
            ConnectorError::Sync(format!(
                "colissimo /idships/{{idship}} JSON parse failed: {e} (body prefix: {})",
                String::from_utf8_lossy(&resp.body[..resp.body.len().min(256)])
            ))
        })?;
        // returnCode 200 with a shipment is a hit; anything else (e.g.
        // returnCode for a not-yet-scanned parcel) yields no record.
        match parsed.shipment {
            Some(s) if parsed.return_code.unwrap_or(200) == 200 => Ok(Some(s)),
            _ => Ok(None),
        }
    }
}

/// Attach the auth header matching the token's provenance: a static
/// Okapi key (tagged [`API_KEY_TOKEN_TYPE`] in `authenticate`) goes in
/// the provider-native `X-Okapi-Key` header, while an OAuth-issued token
/// is sent as `Authorization: <scheme> <token>` (scheme from
/// `token_type`, defaulting to `Bearer`).
fn apply_auth(req: HttpRequest, token: &OAuth2Token) -> HttpRequest {
    apply_auth_by_provenance(req, token, "X-Okapi-Key", API_KEY_TOKEN_TYPE)
}

fn parse_rfc3339(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

fn id_value_to_string(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) if !s.is_empty() => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

impl Connector for ColissimoConnector {
    fn authenticate(&self, config: &ConnectorConfig) -> Result<OAuth2Token> {
        if let Some(api_key) = config
            .auth_config_json
            .get("api_key")
            .and_then(serde_json::Value::as_str)
        {
            let mut token = OAuth2Token::new_without_refresh(
                api_key,
                Utc::now() + chrono::Duration::days(365),
                DEFAULT_SCOPE,
            );
            token.token_type = API_KEY_TOKEN_TYPE.to_string();
            return Ok(token);
        }
        let auth_code = config
            .auth_config_json
            .get("authorization_code")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ConnectorError::Auth(
                    "colissimo authenticate: auth_config_json.api_key or .authorization_code is required"
                        .into(),
                )
            })?;
        self.oauth.exchange_code(config, auth_code)
    }

    fn initial_sync(&self, config: &ConnectorConfig, token: &OAuth2Token) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        let idships = Self::configured_idships(config)?;
        let mut events = Vec::with_capacity(idships.len());
        let mut watermark: Option<DateTime<Utc>> = None;
        for idship in &idships {
            let Some(shipment) = self.fetch_shipment(&base_url, idship, token)? else {
                continue;
            };
            let occurred_at = shipment.latest_event_at().unwrap_or_else(Utc::now);
            events.push(ConnectorEvent::DocumentCreated {
                document_id: SourceDocumentId::new(idship.clone()),
                occurred_at,
            });
            if let Some(t) = shipment.latest_event_at() {
                watermark = Some(watermark.map_or(t, |w| w.max(t)));
            }
        }
        Ok(SyncRunResult {
            events,
            next_cursor: watermark.map(|t| t.to_rfc3339()),
        })
    }

    fn incremental_sync(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        state: &SyncState,
    ) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        let idships = Self::configured_idships(config)?;
        let prior: Option<DateTime<Utc>> = state.cursor.as_deref().and_then(parse_rfc3339);
        let mut events = Vec::new();
        let mut watermark = prior;
        for idship in &idships {
            let Some(shipment) = self.fetch_shipment(&base_url, idship, token)? else {
                continue;
            };
            let Some(updated) = shipment.latest_event_at() else {
                continue;
            };
            if prior.is_some_and(|p| updated <= p) {
                continue;
            }
            events.push(ConnectorEvent::DocumentUpdated {
                document_id: SourceDocumentId::new(idship.clone()),
                occurred_at: updated,
            });
            watermark = Some(watermark.map_or(updated, |w| w.max(updated)));
        }
        Ok(SyncRunResult {
            events,
            next_cursor: watermark.map(|t| t.to_rfc3339()),
        })
    }

    fn fetch_content(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        document_id: &SourceDocumentId,
    ) -> Result<FetchedContent> {
        use std::fmt::Write as _;
        let base_url = self.resolved_base_url(config);
        let idship = document_id.as_str();
        let shipment = self
            .fetch_shipment(&base_url, idship, token)?
            .ok_or_else(|| ConnectorError::Sync(format!("colissimo parcel {idship} not found")))?;
        let product = shipment.product.as_deref().unwrap_or("colissimo");
        let mut body = format!("# Colissimo parcel {idship}\n\nProduct: {product}\n");
        if let Some(is_final) = shipment.is_final {
            let _ = writeln!(body, "Delivered: {is_final}");
        }
        if !shipment.event.is_empty() {
            body.push_str("\n## Tracking history\n");
            for ev in &shipment.event {
                let date = ev.date.as_deref().unwrap_or("");
                let label = ev.label.as_deref().unwrap_or("");
                let _ = writeln!(body, "- {date} {label}");
            }
        }
        Ok(FetchedContent::text(body, "text/markdown")
            .with_title(format!("Colissimo parcel {idship}"))
            .with_metadata(serde_json::json!({
                "provider": "colissimo",
                "record_id": shipment.id_ship,
                "product": shipment.product,
                "is_final": shipment.is_final,
            })))
    }

    fn subscribe_webhook(
        &self,
        config: &ConnectorConfig,
        _token: &OAuth2Token,
        callback_url: &str,
    ) -> Result<WebhookSubscription> {
        // Colissimo tracking webhooks are configured in the provider
        // dashboard; no REST endpoint creates them. Record a polling-only
        // subscription so the runtime falls back to incremental_sync.
        let secret = config
            .auth_config_json
            .get("webhook_secret")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("colissimo-webhook-secret");
        Ok(WebhookSubscription::new(
            self.instance,
            callback_url,
            WebhookSecret::new(secret),
            WebhookEventTypes::all(),
            None,
        ))
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        let deliveries: Vec<ColissimoWebhookEvent> =
            if let Ok(batch) = serde_json::from_slice::<Vec<ColissimoWebhookEvent>>(body) {
                batch
            } else {
                vec![
                    serde_json::from_slice::<ColissimoWebhookEvent>(body).map_err(|e| {
                        ConnectorError::Webhook(format!("colissimo webhook parse failed: {e}"))
                    })?,
                ]
            };
        if deliveries.is_empty() {
            return Err(ConnectorError::Webhook(
                "empty colissimo webhook batch".into(),
            ));
        }
        let mut events = Vec::with_capacity(deliveries.len());
        for delivery in deliveries {
            let id_str = id_value_to_string(&delivery.id_ship).ok_or_else(|| {
                ConnectorError::Webhook("colissimo webhook event missing idShip".into())
            })?;
            let id = SourceDocumentId::new(id_str);
            let occurred_at = Utc::now();
            let event_type = delivery.event.to_ascii_lowercase();
            let event = if event_type.contains("create") {
                ConnectorEvent::DocumentCreated {
                    document_id: id,
                    occurred_at,
                }
            } else if event_type.contains("deliver")
                || event_type.contains("delete")
                || event_type.contains("return")
            {
                // A delivered/returned parcel is terminal; emit an update
                // (the document still exists) unless explicitly deleted.
                if event_type.contains("delete") {
                    ConnectorEvent::DocumentDeleted {
                        document_id: id,
                        occurred_at,
                    }
                } else {
                    ConnectorEvent::DocumentUpdated {
                        document_id: id,
                        occurred_at,
                    }
                }
            } else {
                ConnectorEvent::DocumentUpdated {
                    document_id: id,
                    occurred_at,
                }
            };
            events.push(event);
        }
        Ok(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use connector_framework::{
        AuthKind, ConnectorKind, HttpMethod, MockHttpTransport, MockResponse,
    };
    use evidence_store::ScopeId;

    struct FixedOAuth;
    impl OAuth2CodeExchange for FixedOAuth {
        fn exchange_code(&self, _config: &ConnectorConfig, _code: &str) -> Result<OAuth2Token> {
            Ok(OAuth2Token::new(
                "unused",
                "unused",
                Utc::now() + Duration::hours(1),
                "suivi",
            ))
        }
    }

    fn oauth() -> Arc<dyn OAuth2CodeExchange> {
        Arc::new(FixedOAuth)
    }

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(
            ConnectorKind::Colissimo,
            AuthKind::ApiKey,
            ScopeId::new_v4(),
        )
        .with_auth_config(serde_json::json!({
            "api_key": "okapi-key",
            "api_base_url": "https://api.test/suivi",
            "idships": "6A111, 6A222",
            "webhook_secret": "colissimo-secret",
        }))
    }

    fn cfg_oauth() -> ConnectorConfig {
        ConnectorConfig::new(
            ConnectorKind::Colissimo,
            AuthKind::OAuth2,
            ScopeId::new_v4(),
        )
        .with_auth_config(serde_json::json!({
            "authorization_code": "auth-code",
            "api_base_url": "https://api.test/suivi",
            "idships": "6A111",
            "webhook_secret": "colissimo-secret",
        }))
    }

    fn ok_json(value: &serde_json::Value) -> MockResponse {
        MockResponse::ok_json(serde_json::to_vec(value).unwrap())
    }

    fn shipment_json(id: &str, last_event: &str) -> serde_json::Value {
        serde_json::json!({
            "lang": "fr_FR",
            "returnCode": 200,
            "shipment": {
                "idShip": id,
                "product": "colissimo",
                "isFinal": false,
                "entryDate": "2024-01-01T08:00:00+00:00",
                "event": [
                    {"code": "DR1", "label": "Pris en charge", "date": "2024-01-01T08:00:00+00:00"},
                    {"code": "DI1", "label": "En cours", "date": last_event}
                ]
            }
        })
    }

    #[test]
    fn authenticate_reads_api_key() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = ColissimoConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let token = c.authenticate(&cfg()).unwrap();
        assert_eq!(token.access_token.expose(), "okapi-key");
        assert!(token.refresh_token.is_none());
        assert_eq!(token.token_type, API_KEY_TOKEN_TYPE);
    }

    #[test]
    fn authenticate_falls_back_to_oauth_code() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = ColissimoConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let token = c.authenticate(&cfg_oauth()).unwrap();
        assert_eq!(token.access_token.expose(), "unused");
        assert_eq!(token.token_type, "Bearer");
    }

    #[test]
    fn authenticate_requires_credential() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = ColissimoConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let bare = ConnectorConfig::new(
            ConnectorKind::Colissimo,
            AuthKind::ApiKey,
            ScopeId::new_v4(),
        );
        assert!(matches!(
            c.authenticate(&bare),
            Err(ConnectorError::Auth(_))
        ));
    }

    #[test]
    fn initial_sync_requires_idships() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = ColissimoConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let cfg = ConnectorConfig::new(
            ConnectorKind::Colissimo,
            AuthKind::ApiKey,
            ScopeId::new_v4(),
        )
        .with_auth_config(serde_json::json!({
            "api_key": "okapi-key",
            "api_base_url": "https://api.test/suivi",
        }));
        let tok = c.authenticate(&cfg).unwrap();
        assert!(matches!(
            c.initial_sync(&cfg, &tok),
            Err(ConnectorError::Sync(_))
        ));
    }

    #[test]
    fn initial_sync_fetches_each_parcel_and_sends_okapi_key() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/suivi/idships/6A111",
            ok_json(&shipment_json("6A111", "2024-01-02T10:00:00+00:00")),
        );
        transport.expect(
            HttpMethod::Get,
            "https://api.test/suivi/idships/6A222",
            ok_json(&shipment_json("6A222", "2024-01-03T10:00:00+00:00")),
        );
        let c = ColissimoConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 2);
        assert!(matches!(
            res.events[0],
            ConnectorEvent::DocumentCreated { .. }
        ));
        // Watermark is the latest event across all parcels.
        assert_eq!(
            res.next_cursor.as_deref(),
            Some("2024-01-03T10:00:00+00:00")
        );
        // Auth header is X-Okapi-Key, never the custom X-Colissimo-Api-Key.
        let recorded = transport.recorded();
        assert!(recorded[0]
            .headers
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("X-Okapi-Key") && v == "okapi-key"));
        assert!(!recorded[0]
            .headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("X-Colissimo-Api-Key")));
    }

    #[test]
    fn initial_sync_skips_unknown_parcel() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/suivi/idships/6A111",
            ok_json(&shipment_json("6A111", "2024-01-02T10:00:00+00:00")),
        );
        transport.expect(
            HttpMethod::Get,
            "https://api.test/suivi/idships/6A222",
            MockResponse::status(404, b"{\"returnCode\":404}".to_vec()),
        );
        let c = ColissimoConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        // The 404 parcel is skipped, not fatal.
        assert_eq!(res.events.len(), 1);
    }

    #[test]
    fn incremental_sync_emits_only_advanced_parcels() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/suivi/idships/6A111",
            ok_json(&shipment_json("6A111", "2024-01-02T10:00:00+00:00")),
        );
        transport.expect(
            HttpMethod::Get,
            "https://api.test/suivi/idships/6A222",
            ok_json(&shipment_json("6A222", "2024-01-05T10:00:00+00:00")),
        );
        let c = ColissimoConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let mut state = SyncState::new(c.instance);
        state.cursor = Some("2024-01-03T00:00:00+00:00".to_string());
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        // 6A111's latest event (Jan 2) is <= cursor → skipped; 6A222 (Jan 5) advanced.
        assert_eq!(res.events.len(), 1);
        assert!(matches!(
            res.events[0],
            ConnectorEvent::DocumentUpdated { .. }
        ));
        assert_eq!(
            res.next_cursor.as_deref(),
            Some("2024-01-05T10:00:00+00:00")
        );
    }

    #[test]
    fn fetch_content_renders_tracking_history() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/suivi/idships/6A111",
            ok_json(&shipment_json("6A111", "2024-01-02T10:00:00+00:00")),
        );
        let c = ColissimoConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let content = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("6A111"))
            .unwrap();
        let text = String::from_utf8(content.body).unwrap();
        assert!(text.contains("Colissimo parcel 6A111"));
        assert!(text.contains("Pris en charge"));
        assert_eq!(content.title.as_deref(), Some("Colissimo parcel 6A111"));
    }

    #[test]
    fn subscribe_webhook_is_polling_only() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = ColissimoConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let sub = c
            .subscribe_webhook(&cfg(), &tok, "https://callback.test/colissimo")
            .unwrap();
        assert_eq!(sub.callback_url, "https://callback.test/colissimo");
        assert!(sub.provider_subscription_id.is_none());
    }

    #[test]
    fn handle_webhook_event_parses_idship() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = ColissimoConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let body = serde_json::to_vec(&serde_json::json!({
            "idShip": "6A111",
            "eventType": "parcel.delivered"
        }))
        .unwrap();
        let events = c.handle_webhook_event(&body).unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ConnectorEvent::DocumentUpdated { document_id, .. } => {
                assert_eq!(document_id.as_str(), "6A111");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn production_base_url_does_not_duplicate_version() {
        // Exercises the real DEFAULT_API_BASE_URL (the circular tests
        // override it). The Suivi v2 host already carries the version, so
        // the parcel path must not invent a `/v1/` prefix.
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.laposte.fr/suivi/v2/idships/6A111",
            ok_json(&shipment_json("6A111", "2024-01-02T10:00:00+00:00")),
        );
        let prod_cfg = ConnectorConfig::new(
            ConnectorKind::Colissimo,
            AuthKind::ApiKey,
            ScopeId::new_v4(),
        )
        .with_auth_config(serde_json::json!({
            "api_key": "okapi-key",
            "idships": "6A111",
        }));
        let c = ColissimoConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&prod_cfg).unwrap();
        let res = c.initial_sync(&prod_cfg, &tok).unwrap();
        assert_eq!(res.events.len(), 1);
        let recorded = transport.recorded();
        assert_eq!(
            recorded[0].url,
            "https://api.laposte.fr/suivi/v2/idships/6A111"
        );
    }
}
