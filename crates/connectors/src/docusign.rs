//! DocuSign connector — eSignature REST API + Connect webhooks.
//!
//! * `initial_sync` walks
//!   `GET /restapi/v2.1/accounts/{accountId}/envelopes` from a far
//!   past `from_date` and pages via `start_position`.
//! * `incremental_sync` sets `from_date` to the prior watermark and
//!   dedupes the inclusive boundary envelope.
//! * `fetch_content` reads a single envelope and renders a Markdown
//!   summary.
//! * `subscribe_webhook` POSTs `/restapi/v2.1/accounts/{accountId}/connect`
//!   to register a Connect configuration.
//! * `handle_webhook_event` parses a Connect event payload; DocuSign
//!   delivers one envelope event per notification, with the lifecycle
//!   in the `event` field.
//!
//! DocuSign authenticates with OAuth2 bearer tokens, so the bearer
//! helpers apply directly. `authenticate` accepts a configured
//! `access_token` or an OAuth2 `authorization_code`.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use connector_framework::{
    bearer_get_json, bearer_post_json, percent_encode_path_component, Connector, ConnectorConfig,
    ConnectorError, ConnectorEvent, ConnectorInstanceId, FetchedContent, HttpTransport,
    OAuth2CodeExchange, OAuth2Token, Result, SourceDocumentId, SyncRunResult, SyncState,
    WebhookEventTypes, WebhookSecret, WebhookSubscription,
};
use serde::{Deserialize, Serialize};

/// Default DocuSign eSignature base URL (demo environment). Production
/// callers override this with their account base URI.
pub const DEFAULT_API_BASE_URL: &str = "https://demo.docusign.net";

/// Page size for the envelopes list. DocuSign caps results per page.
pub const DEFAULT_PAGE_SIZE: u32 = 100;

/// Far-past `from_date` used for the initial full walk.
pub const INITIAL_FROM_DATE: &str = "2000-01-01T00:00:00Z";

/// Safety ceiling on list pages walked in one sync run.
pub const MAX_LIST_PAGES: usize = 10_000;

/// Scope recorded on a token synthesised from a configured token.
const DEFAULT_SCOPE: &str = "signature";

/// One DocuSign envelope (subset).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DocuSignEnvelope {
    /// Envelope id (GUID).
    #[serde(rename = "envelopeId", default)]
    pub envelope_id: String,
    /// Lifecycle status (`sent`, `delivered`, `completed`, `voided`).
    #[serde(default)]
    pub status: Option<String>,
    /// Subject line.
    #[serde(rename = "emailSubject", default)]
    pub email_subject: Option<String>,
    /// Creation timestamp.
    #[serde(rename = "createdDateTime", default)]
    pub created_date_time: Option<DateTime<Utc>>,
    /// Timestamp of the last status change.
    #[serde(rename = "statusChangedDateTime", default)]
    pub status_changed_date_time: Option<DateTime<Utc>>,
}

/// One page of the envelopes list.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DocuSignEnvelopesResponse {
    /// Envelopes on this page.
    #[serde(default)]
    pub envelopes: Vec<DocuSignEnvelope>,
    /// Total result-set size (as a string in the API).
    #[serde(rename = "totalSetSize", default)]
    pub total_set_size: Option<String>,
    /// Start position of this page (as a string).
    #[serde(rename = "resultSetSize", default)]
    pub result_set_size: Option<String>,
    /// Absolute URI of the next page, when present.
    #[serde(rename = "nextUri", default)]
    pub next_uri: Option<String>,
}

/// Response from `POST /restapi/v2.1/accounts/{accountId}/connect`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DocuSignConnectResponse {
    /// Connect configuration id.
    #[serde(rename = "connectId", default)]
    pub connect_id: String,
}

/// Connect webhook payload (one envelope event per notification).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DocuSignWebhookPayload {
    /// Lifecycle event (`envelope-sent`, `envelope-completed`,
    /// `envelope-voided`, …).
    #[serde(default)]
    pub event: Option<String>,
    /// Event data block.
    #[serde(default)]
    pub data: Option<DocuSignWebhookData>,
}

/// `data` block of a Connect notification.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DocuSignWebhookData {
    /// The envelope id the event concerns.
    #[serde(rename = "envelopeId", default)]
    pub envelope_id: String,
}

/// DocuSign connector.
pub struct DocuSignConnector {
    /// Connector instance id.
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    oauth: Arc<dyn OAuth2CodeExchange>,
    api_base_url: String,
    page_size: u32,
}

impl std::fmt::Debug for DocuSignConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DocuSignConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("page_size", &self.page_size)
            .field("transport", &"<HttpTransport>")
            .field("oauth", &"<OAuth2CodeExchange>")
            .finish()
    }
}

impl DocuSignConnector {
    /// Construct a DocuSign connector.
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
            page_size: DEFAULT_PAGE_SIZE,
        }
    }

    /// Override the DocuSign base URL (account base URI).
    #[must_use]
    pub fn with_api_base_url(mut self, url: impl Into<String>) -> Self {
        self.api_base_url = url.into();
        self
    }

    /// Override the list page size. Clamped to `[1, 1000]`.
    #[must_use]
    pub fn with_page_size(mut self, size: u32) -> Self {
        self.page_size = size.clamp(1, 1000);
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

    fn account_id(config: &ConnectorConfig) -> Result<String> {
        config
            .auth_config_json
            .get("account_id")
            .and_then(serde_json::Value::as_str)
            .map(std::string::ToString::to_string)
            .ok_or_else(|| {
                ConnectorError::Sync("docusign: auth_config_json.account_id is required".into())
            })
    }

    /// Walk every envelopes page, advancing `start_position` until a
    /// short page is returned.
    fn paginate_envelopes(
        &self,
        base_url: &str,
        account_enc: &str,
        token: &OAuth2Token,
        from_date: &str,
    ) -> Result<Vec<DocuSignEnvelope>> {
        let mut out = Vec::<DocuSignEnvelope>::new();
        let mut start_position: u32 = 0;
        for _ in 0..MAX_LIST_PAGES {
            let url = format!(
                "{base_url}/restapi/v2.1/accounts/{account_enc}/envelopes?from_date={}&count={}&start_position={start_position}",
                percent_encode_path_component(from_date),
                self.page_size
            );
            let resp: DocuSignEnvelopesResponse = bearer_get_json(
                &self.transport,
                "docusign",
                "/restapi/v2.1/accounts/{id}/envelopes",
                &url,
                token,
                &[],
            )?;
            let returned = resp.envelopes.len();
            out.extend(resp.envelopes);
            if returned < self.page_size as usize {
                return Ok(out);
            }
            start_position =
                start_position.saturating_add(u32::try_from(returned).unwrap_or(u32::MAX));
        }
        Err(ConnectorError::Sync(format!(
            "docusign envelopes exceeded {MAX_LIST_PAGES} pages"
        )))
    }
}

fn envelope_time(e: &DocuSignEnvelope) -> Option<DateTime<Utc>> {
    e.status_changed_date_time.or(e.created_date_time)
}

fn envelope_to_event(e: &DocuSignEnvelope, kind: &str) -> ConnectorEvent {
    let occurred_at = envelope_time(e).unwrap_or_else(Utc::now);
    let id = SourceDocumentId::new(e.envelope_id.clone());
    match kind {
        "create" => ConnectorEvent::DocumentCreated {
            document_id: id,
            occurred_at,
        },
        "delete" => ConnectorEvent::DocumentDeleted {
            document_id: id,
            occurred_at,
        },
        _ => ConnectorEvent::DocumentUpdated {
            document_id: id,
            occurred_at,
        },
    }
}

impl Connector for DocuSignConnector {
    fn authenticate(&self, config: &ConnectorConfig) -> Result<OAuth2Token> {
        if let Some(tok) = config
            .auth_config_json
            .get("access_token")
            .and_then(serde_json::Value::as_str)
        {
            return Ok(OAuth2Token::new_without_refresh(
                tok,
                Utc::now() + chrono::Duration::days(3650),
                DEFAULT_SCOPE,
            ));
        }
        let auth_code = config
            .auth_config_json
            .get("authorization_code")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ConnectorError::Auth(
                    "docusign authenticate: auth_config_json.access_token or .authorization_code is required"
                        .into(),
                )
            })?;
        self.oauth.exchange_code(config, auth_code)
    }

    fn initial_sync(&self, config: &ConnectorConfig, token: &OAuth2Token) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        let account = Self::account_id(config)?;
        let account_enc = percent_encode_path_component(&account);
        let envelopes =
            self.paginate_envelopes(&base_url, &account_enc, token, INITIAL_FROM_DATE)?;
        let mut events = Vec::with_capacity(envelopes.len());
        let mut watermark: Option<DateTime<Utc>> = None;
        for e in &envelopes {
            events.push(envelope_to_event(e, "create"));
            if let Some(t) = envelope_time(e) {
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
        let account = Self::account_id(config)?;
        let account_enc = percent_encode_path_component(&account);
        let prior = state
            .cursor
            .as_deref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc));
        let from_date = prior.map_or_else(|| INITIAL_FROM_DATE.to_string(), |p| p.to_rfc3339());
        let envelopes = self.paginate_envelopes(&base_url, &account_enc, token, &from_date)?;
        let mut events = Vec::new();
        let mut watermark = prior;
        for e in &envelopes {
            let when = envelope_time(e);
            // `from_date` is inclusive; skip the boundary envelope
            // already emitted on the prior run.
            if let (Some(prev), Some(t)) = (prior, when) {
                if t <= prev {
                    continue;
                }
            }
            events.push(envelope_to_event(e, "update"));
            if let Some(t) = when {
                watermark = Some(watermark.map_or(t, |w| w.max(t)));
            }
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
        let base_url = self.resolved_base_url(config);
        let account = Self::account_id(config)?;
        let account_enc = percent_encode_path_component(&account);
        let id_enc = percent_encode_path_component(document_id.as_str());
        let url = format!("{base_url}/restapi/v2.1/accounts/{account_enc}/envelopes/{id_enc}");
        let envelope: DocuSignEnvelope = bearer_get_json(
            &self.transport,
            "docusign",
            "/restapi/v2.1/accounts/{id}/envelopes/{envelopeId}",
            &url,
            token,
            &[],
        )?;

        let title = envelope
            .email_subject
            .clone()
            .unwrap_or_else(|| format!("Envelope {}", document_id.as_str()));
        let mut md = String::new();
        md.push_str("# ");
        md.push_str(&title);
        md.push_str("\n\n");
        if let Some(status) = envelope.status.as_deref().filter(|s| !s.is_empty()) {
            md.push_str("**Status:** ");
            md.push_str(status);
            md.push_str("\n\n");
        }
        let body = md.trim_end().to_string();

        Ok(FetchedContent::text(body, "text/markdown")
            .with_title(title)
            .with_metadata(serde_json::json!({
                "provider": "docusign",
                "envelope_id": envelope.envelope_id,
                "status": envelope.status,
            }))
            .with_source_url(format!(
                "{base_url}/restapi/v2.1/accounts/{account}/envelopes/{}",
                document_id.as_str()
            )))
    }

    fn subscribe_webhook(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        callback_url: &str,
    ) -> Result<WebhookSubscription> {
        let base_url = self.resolved_base_url(config);
        let account = Self::account_id(config)?;
        let account_enc = percent_encode_path_component(&account);
        let url = format!("{base_url}/restapi/v2.1/accounts/{account_enc}/connect");
        let body = serde_json::json!({
            "configurationType": "custom",
            "name": "knowledge-substrate",
            "urlToPublishTo": callback_url,
            "allUsers": "true",
            "envelopeEvents": ["sent", "delivered", "completed", "declined", "voided"],
        });
        let resp: DocuSignConnectResponse = bearer_post_json(
            &self.transport,
            "docusign",
            "/restapi/v2.1/accounts/{id}/connect",
            &url,
            token,
            &[],
            &body,
        )?;
        if resp.connect_id.is_empty() {
            return Err(ConnectorError::Webhook(
                "docusign /restapi/v2.1/accounts/{id}/connect returned no connectId".into(),
            ));
        }
        let mut subscription = WebhookSubscription::new(
            self.instance,
            callback_url,
            WebhookSecret::new(
                config
                    .auth_config_json
                    .get("webhook_secret")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("docusign-webhook-secret"),
            ),
            WebhookEventTypes::all(),
            None,
        );
        subscription.provider_subscription_id = Some(resp.connect_id);
        Ok(subscription)
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        let payload: DocuSignWebhookPayload = serde_json::from_slice(body)?;
        let data = payload
            .data
            .ok_or_else(|| ConnectorError::Webhook("docusign webhook missing data".into()))?;
        if data.envelope_id.is_empty() {
            return Err(ConnectorError::Webhook(
                "docusign webhook missing envelopeId".into(),
            ));
        }
        let id = SourceDocumentId::new(data.envelope_id);
        let occurred_at = Utc::now();
        let event = match payload.event.as_deref() {
            Some("envelope-sent") => ConnectorEvent::DocumentCreated {
                document_id: id,
                occurred_at,
            },
            Some("envelope-voided" | "envelope-declined") => ConnectorEvent::DocumentDeleted {
                document_id: id,
                occurred_at,
            },
            _ => ConnectorEvent::DocumentUpdated {
                document_id: id,
                occurred_at,
            },
        };
        Ok(vec![event])
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
                "ds-access",
                "ds-refresh",
                Utc::now() + Duration::hours(1),
                "signature",
            ))
        }
    }

    fn oauth() -> Arc<dyn OAuth2CodeExchange> {
        Arc::new(FixedOAuth)
    }

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(ConnectorKind::DocuSign, AuthKind::OAuth2, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({
                "access_token": "ds-tok",
                "account_id": "acct1",
                "api_base_url": "https://api.test/ds",
            }))
    }

    fn envelope(id: &str, changed: &str) -> serde_json::Value {
        serde_json::json!({
            "envelopeId": id, "status": "sent",
            "emailSubject": format!("Doc {id}"), "statusChangedDateTime": changed
        })
    }

    fn ok_json(value: &serde_json::Value) -> MockResponse {
        MockResponse::ok_json(serde_json::to_vec(value).unwrap())
    }

    #[test]
    fn authenticate_wraps_access_token() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = DocuSignConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        assert_eq!(
            c.authenticate(&cfg()).unwrap().access_token.expose(),
            "ds-tok"
        );
    }

    #[test]
    fn authenticate_falls_back_to_oauth() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = DocuSignConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let cfg =
            ConnectorConfig::new(ConnectorKind::DocuSign, AuthKind::OAuth2, ScopeId::new_v4())
                .with_auth_config(
                    serde_json::json!({ "authorization_code": "abc", "account_id": "a" }),
                );
        assert_eq!(
            c.authenticate(&cfg).unwrap().access_token.expose(),
            "ds-access"
        );
    }

    #[test]
    fn initial_sync_paginates_via_start_position() {
        let transport = Arc::new(MockHttpTransport::new());
        let full: Vec<serde_json::Value> = (0..100)
            .map(|i| envelope(&format!("e{i}"), "2024-01-01T00:00:00Z"))
            .collect();
        transport.expect(
            HttpMethod::Get,
            format!(
                "https://api.test/ds/restapi/v2.1/accounts/acct1/envelopes?from_date={}&count=100&start_position=0",
                percent_encode_path_component(INITIAL_FROM_DATE)
            ),
            ok_json(&serde_json::json!({ "envelopes": full })),
        );
        transport.expect(
            HttpMethod::Get,
            format!(
                "https://api.test/ds/restapi/v2.1/accounts/acct1/envelopes?from_date={}&count=100&start_position=100",
                percent_encode_path_component(INITIAL_FROM_DATE)
            ),
            ok_json(&serde_json::json!({ "envelopes": [envelope("e100", "2024-01-02T00:00:00Z")] })),
        );
        let c = DocuSignConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 101);
        assert_eq!(transport.recorded().len(), 2);
    }

    #[test]
    fn incremental_sync_uses_from_date_and_dedupes() {
        let transport = Arc::new(MockHttpTransport::new());
        let prior = "2024-01-01T00:00:00+00:00";
        transport.expect(
            HttpMethod::Get,
            format!(
                "https://api.test/ds/restapi/v2.1/accounts/acct1/envelopes?from_date={}&count=100&start_position=0",
                percent_encode_path_component(prior)
            ),
            ok_json(&serde_json::json!({ "envelopes": [
                envelope("old", "2024-01-01T00:00:00Z"),
                envelope("new", "2024-02-01T00:00:00Z"),
            ] })),
        );
        let c = DocuSignConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let mut state = SyncState::new(c.instance);
        state.cursor = Some(prior.to_string());
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        assert_eq!(res.events.len(), 1);
        assert_eq!(res.events[0].document_id().as_str(), "new");
    }

    #[test]
    fn initial_sync_requires_account_id() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = DocuSignConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let cfg =
            ConnectorConfig::new(ConnectorKind::DocuSign, AuthKind::OAuth2, ScopeId::new_v4())
                .with_auth_config(serde_json::json!({ "access_token": "t" }));
        let tok = c.authenticate(&cfg).unwrap();
        assert!(matches!(
            c.initial_sync(&cfg, &tok).unwrap_err(),
            ConnectorError::Sync(_)
        ));
    }

    #[test]
    fn subscribe_webhook_captures_connect_id() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            "https://api.test/ds/restapi/v2.1/accounts/acct1/connect",
            ok_json(&serde_json::json!({ "connectId": "conn-9" })),
        );
        let c = DocuSignConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let sub = c
            .subscribe_webhook(&cfg(), &tok, "https://hook.example/ds")
            .unwrap();
        assert_eq!(sub.provider_subscription_id.as_deref(), Some("conn-9"));
    }

    #[test]
    fn webhook_lifecycle_maps_correctly() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = DocuSignConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let mk =
            |event: &str| serde_json::json!({ "event": event, "data": { "envelopeId": "env1" } });
        assert!(matches!(
            c.handle_webhook_event(&serde_json::to_vec(&mk("envelope-sent")).unwrap())
                .unwrap()[0],
            ConnectorEvent::DocumentCreated { .. }
        ));
        assert!(matches!(
            c.handle_webhook_event(&serde_json::to_vec(&mk("envelope-completed")).unwrap())
                .unwrap()[0],
            ConnectorEvent::DocumentUpdated { .. }
        ));
        assert!(matches!(
            c.handle_webhook_event(&serde_json::to_vec(&mk("envelope-voided")).unwrap())
                .unwrap()[0],
            ConnectorEvent::DocumentDeleted { .. }
        ));
    }

    #[test]
    fn webhook_missing_data_errors() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = DocuSignConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let body = serde_json::json!({ "event": "envelope-sent" });
        assert!(matches!(
            c.handle_webhook_event(&serde_json::to_vec(&body).unwrap())
                .unwrap_err(),
            ConnectorError::Webhook(_)
        ));
    }

    #[test]
    fn fetch_content_renders_summary() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/ds/restapi/v2.1/accounts/acct1/envelopes/env7",
            ok_json(&serde_json::json!({
                "envelopeId": "env7", "status": "completed", "emailSubject": "Sign here"
            })),
        );
        let c = DocuSignConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let fc = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("env7"))
            .unwrap();
        let body = String::from_utf8(fc.body).unwrap();
        assert!(body.contains("# Sign here"));
        assert!(body.contains("**Status:** completed"));
    }

    #[test]
    fn fetch_content_maps_404_to_sync_error() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/ds/restapi/v2.1/accounts/acct1/envelopes/none",
            MockResponse::status(404, b"not found".to_vec()),
        );
        let c = DocuSignConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        assert!(matches!(
            c.fetch_content(&cfg(), &tok, &SourceDocumentId::new("none"))
                .unwrap_err(),
            ConnectorError::Sync(_)
        ));
    }
}
