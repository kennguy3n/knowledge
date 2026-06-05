//! QuickBooks Online connector — Accounting API + Intuit webhooks.
//!
//! * `initial_sync` issues `GET /v3/company/{realmId}/query` with a
//!   `SELECT * FROM Customer` statement and pages via the query's
//!   `STARTPOSITION` / `MAXRESULTS` clauses.
//! * `incremental_sync` adds a
//!   `WHERE MetaData.LastUpdatedTime > '<iso>'` predicate keyed off
//!   the prior watermark.
//! * `fetch_content` reads `GET /v3/company/{realmId}/customer/{id}`
//!   and renders a Markdown summary.
//! * `subscribe_webhook` does **not** call the API — Intuit webhooks
//!   are configured once in the developer portal, so the connector
//!   surfaces the operator-provided verifier token as the
//!   subscription secret.
//! * `handle_webhook_event` parses Intuit's batched
//!   `eventNotifications` payload and emits **every** entity change.
//!
//! QuickBooks authenticates with OAuth2 bearer tokens, so the bearer
//! helpers apply directly. `authenticate` accepts a configured
//! `access_token` or an OAuth2 `authorization_code`.

use std::fmt::Write as _;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use connector_framework::{
    bearer_get_json, percent_encode_path_component, Connector, ConnectorConfig, ConnectorError,
    ConnectorEvent, ConnectorInstanceId, FetchedContent, HttpTransport, OAuth2CodeExchange,
    OAuth2Token, Result, SourceDocumentId, SyncRunResult, SyncState, WebhookEventTypes,
    WebhookSecret, WebhookSubscription,
};
use serde::{Deserialize, Serialize};

/// Default QuickBooks Online Accounting API base URL (production).
pub const DEFAULT_API_BASE_URL: &str = "https://quickbooks.api.intuit.com";

/// Accounting API minor version pinned by this connector.
pub const MINOR_VERSION: &str = "65";

/// Page size (`MAXRESULTS`). QuickBooks caps query results at 1000.
pub const DEFAULT_PAGE_SIZE: u32 = 100;

/// Safety ceiling on query pages walked in one sync run.
pub const MAX_LIST_PAGES: usize = 10_000;

/// Scope recorded on a token synthesised from a configured token.
const DEFAULT_SCOPE: &str = "com.intuit.quickbooks.accounting";

/// Metadata block on a QuickBooks entity.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QuickBooksMetaData {
    /// Entity creation time.
    #[serde(rename = "CreateTime", default)]
    pub create_time: Option<DateTime<Utc>>,
    /// Entity last-update time.
    #[serde(rename = "LastUpdatedTime", default)]
    pub last_updated_time: Option<DateTime<Utc>>,
}

/// One QuickBooks customer (subset).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QuickBooksCustomer {
    /// Entity id.
    #[serde(rename = "Id", default)]
    pub id: String,
    /// Display name.
    #[serde(rename = "DisplayName", default)]
    pub display_name: Option<String>,
    /// Primary email record (`PrimaryEmailAddr.Address`).
    #[serde(rename = "PrimaryEmailAddr", default)]
    pub primary_email_addr: Option<QuickBooksEmail>,
    /// Entity metadata (timestamps).
    #[serde(rename = "MetaData", default)]
    pub meta_data: Option<QuickBooksMetaData>,
}

/// Email sub-record.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QuickBooksEmail {
    /// Email address.
    #[serde(rename = "Address", default)]
    pub address: Option<String>,
}

/// `QueryResponse` envelope.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QuickBooksQueryResponse {
    /// Customers returned by the query.
    #[serde(rename = "Customer", default)]
    pub customer: Vec<QuickBooksCustomer>,
    /// Start position echoed by the API.
    #[serde(rename = "startPosition", default)]
    pub start_position: Option<i64>,
    /// Maximum results echoed by the API.
    #[serde(rename = "maxResults", default)]
    pub max_results: Option<i64>,
}

/// Top-level query response (`{QueryResponse:{…}}`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QuickBooksQueryEnvelope {
    /// The query response block.
    #[serde(rename = "QueryResponse", default)]
    pub query_response: QuickBooksQueryResponse,
}

/// Single-customer response (`{Customer:{…}}`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QuickBooksCustomerEnvelope {
    /// The customer.
    #[serde(rename = "Customer", default)]
    pub customer: QuickBooksCustomer,
}

/// Intuit webhook payload (`{eventNotifications:[…]}`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QuickBooksWebhookPayload {
    /// Per-realm event notifications.
    #[serde(rename = "eventNotifications", default)]
    pub event_notifications: Vec<QuickBooksEventNotification>,
}

/// One realm's event notification.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QuickBooksEventNotification {
    /// Realm (company) id.
    #[serde(rename = "realmId", default)]
    pub realm_id: String,
    /// Data-change event block.
    #[serde(rename = "dataChangeEvent", default)]
    pub data_change_event: Option<QuickBooksDataChangeEvent>,
}

/// `dataChangeEvent` block.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QuickBooksDataChangeEvent {
    /// Changed entities.
    #[serde(default)]
    pub entities: Vec<QuickBooksEntity>,
}

/// One changed entity.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QuickBooksEntity {
    /// Entity name (e.g. `Customer`).
    #[serde(default)]
    pub name: Option<String>,
    /// Entity id.
    #[serde(default)]
    pub id: String,
    /// Operation (`Create`, `Update`, `Delete`, `Merge`, `Void`).
    #[serde(default)]
    pub operation: Option<String>,
    /// Timestamp of the change.
    #[serde(rename = "lastUpdated", default)]
    pub last_updated: Option<DateTime<Utc>>,
}

/// QuickBooks Online connector.
pub struct QuickBooksConnector {
    /// Connector instance id.
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    oauth: Arc<dyn OAuth2CodeExchange>,
    api_base_url: String,
    page_size: u32,
}

impl std::fmt::Debug for QuickBooksConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QuickBooksConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("page_size", &self.page_size)
            .field("transport", &"<HttpTransport>")
            .field("oauth", &"<OAuth2CodeExchange>")
            .finish()
    }
}

impl QuickBooksConnector {
    /// Construct a QuickBooks connector.
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

    /// Override the Accounting API base URL (e.g. the sandbox host).
    #[must_use]
    pub fn with_api_base_url(mut self, url: impl Into<String>) -> Self {
        self.api_base_url = url.into();
        self
    }

    /// Override the query page size. Clamped to `[1, 1000]`.
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

    fn realm_id(config: &ConnectorConfig) -> Result<String> {
        config
            .auth_config_json
            .get("realm_id")
            .and_then(serde_json::Value::as_str)
            .map(std::string::ToString::to_string)
            .ok_or_else(|| {
                ConnectorError::Sync("quickbooks: auth_config_json.realm_id is required".into())
            })
    }

    /// Walk every query page, advancing `STARTPOSITION` until a short
    /// page is returned. QuickBooks `STARTPOSITION` is 1-based.
    fn paginate_customers(
        &self,
        base_url: &str,
        realm_enc: &str,
        token: &OAuth2Token,
        where_clause: Option<&str>,
    ) -> Result<Vec<QuickBooksCustomer>> {
        let mut out = Vec::<QuickBooksCustomer>::new();
        let mut start_position: u32 = 1;
        for _ in 0..MAX_LIST_PAGES {
            let mut query = String::from("SELECT * FROM Customer");
            if let Some(w) = where_clause {
                query.push_str(" WHERE ");
                query.push_str(w);
            }
            let _ = write!(
                query,
                " ORDERBY MetaData.LastUpdatedTime ASC STARTPOSITION {start_position} MAXRESULTS {}",
                self.page_size
            );
            let url = format!(
                "{base_url}/v3/company/{realm_enc}/query?query={}&minorversion={MINOR_VERSION}",
                percent_encode_path_component(&query)
            );
            let envelope: QuickBooksQueryEnvelope = bearer_get_json(
                &self.transport,
                "quickbooks",
                "/v3/company/{realmId}/query",
                &url,
                token,
                &[],
            )?;
            let returned = envelope.query_response.customer.len();
            out.extend(envelope.query_response.customer);
            if returned < self.page_size as usize {
                return Ok(out);
            }
            start_position =
                start_position.saturating_add(u32::try_from(returned).unwrap_or(u32::MAX));
        }
        Err(ConnectorError::Sync(format!(
            "quickbooks query exceeded {MAX_LIST_PAGES} pages"
        )))
    }
}

fn customer_time(c: &QuickBooksCustomer) -> Option<DateTime<Utc>> {
    c.meta_data
        .as_ref()
        .and_then(|m| m.last_updated_time.or(m.create_time))
}

fn customer_to_event(c: &QuickBooksCustomer, kind: &str) -> ConnectorEvent {
    let occurred_at = customer_time(c).unwrap_or_else(Utc::now);
    let id = SourceDocumentId::new(c.id.clone());
    match kind {
        "create" => ConnectorEvent::DocumentCreated {
            document_id: id,
            occurred_at,
        },
        _ => ConnectorEvent::DocumentUpdated {
            document_id: id,
            occurred_at,
        },
    }
}

impl Connector for QuickBooksConnector {
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
                    "quickbooks authenticate: auth_config_json.access_token or .authorization_code is required"
                        .into(),
                )
            })?;
        self.oauth.exchange_code(config, auth_code)
    }

    fn initial_sync(&self, config: &ConnectorConfig, token: &OAuth2Token) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        let realm = Self::realm_id(config)?;
        let realm_enc = percent_encode_path_component(&realm);
        let customers = self.paginate_customers(&base_url, &realm_enc, token, None)?;
        let mut events = Vec::with_capacity(customers.len());
        let mut watermark: Option<DateTime<Utc>> = None;
        for c in &customers {
            events.push(customer_to_event(c, "create"));
            if let Some(t) = customer_time(c) {
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
        let realm = Self::realm_id(config)?;
        let realm_enc = percent_encode_path_component(&realm);
        let prior = state
            .cursor
            .as_deref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc));
        let where_clause =
            prior.map(|p| format!("MetaData.LastUpdatedTime > '{}'", p.to_rfc3339()));
        let customers =
            self.paginate_customers(&base_url, &realm_enc, token, where_clause.as_deref())?;
        let mut events = Vec::new();
        let mut watermark = prior;
        for c in &customers {
            let when = customer_time(c);
            // The `>` predicate is exclusive, but guard the boundary
            // anyway so a record at the cursor is never re-emitted.
            if let (Some(prev), Some(t)) = (prior, when) {
                if t <= prev {
                    continue;
                }
            }
            events.push(customer_to_event(c, "update"));
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
        let realm = Self::realm_id(config)?;
        let realm_enc = percent_encode_path_component(&realm);
        let id_enc = percent_encode_path_component(document_id.as_str());
        let url = format!(
            "{base_url}/v3/company/{realm_enc}/customer/{id_enc}?minorversion={MINOR_VERSION}"
        );
        let envelope: QuickBooksCustomerEnvelope = bearer_get_json(
            &self.transport,
            "quickbooks",
            "/v3/company/{realmId}/customer/{id}",
            &url,
            token,
            &[],
        )?;
        let customer = envelope.customer;

        let title = customer
            .display_name
            .clone()
            .unwrap_or_else(|| format!("Customer {}", document_id.as_str()));
        let mut md = String::new();
        md.push_str("# ");
        md.push_str(&title);
        md.push_str("\n\n");
        if let Some(email) = customer
            .primary_email_addr
            .as_ref()
            .and_then(|e| e.address.as_deref())
            .filter(|s| !s.is_empty())
        {
            md.push_str("**Email:** ");
            md.push_str(email);
            md.push_str("\n\n");
        }
        let body = md.trim_end().to_string();

        Ok(FetchedContent::text(body, "text/markdown")
            .with_title(title)
            .with_metadata(serde_json::json!({
                "provider": "quickbooks",
                "realm_id": realm,
                "customer_id": customer.id,
            }))
            .with_source_url(format!(
                "{base_url}/v3/company/{realm}/customer/{}",
                document_id.as_str()
            )))
    }

    fn subscribe_webhook(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        callback_url: &str,
    ) -> Result<WebhookSubscription> {
        // Intuit webhooks are configured once in the developer portal
        // (there is no create-webhook REST endpoint), so we do not
        // issue an HTTP call here. We surface the operator-provided
        // verifier token as the signing secret so the substrate can
        // validate the `intuit-signature` HMAC on delivery.
        let _ = token;
        let secret = config
            .auth_config_json
            .get("webhook_verifier_token")
            .or_else(|| config.auth_config_json.get("webhook_secret"))
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ConnectorError::Webhook(
                    "quickbooks subscribe_webhook: auth_config_json.webhook_verifier_token is required"
                        .into(),
                )
            })?;
        let mut subscription = WebhookSubscription::new(
            self.instance,
            callback_url,
            WebhookSecret::new(secret),
            WebhookEventTypes::all(),
            None,
        );
        // The provider id is the realm the notifications are scoped to.
        if let Ok(realm) = Self::realm_id(config) {
            subscription.provider_subscription_id = Some(realm);
        }
        Ok(subscription)
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        let payload: QuickBooksWebhookPayload = serde_json::from_slice(body)?;
        let mut events = Vec::new();
        for notification in &payload.event_notifications {
            let Some(change) = notification.data_change_event.as_ref() else {
                continue;
            };
            for entity in &change.entities {
                if entity.id.is_empty() {
                    continue;
                }
                let occurred_at = entity.last_updated.unwrap_or_else(Utc::now);
                let id = SourceDocumentId::new(entity.id.clone());
                let mapped = match entity.operation.as_deref() {
                    Some("Create") => ConnectorEvent::DocumentCreated {
                        document_id: id,
                        occurred_at,
                    },
                    Some("Delete" | "Void") => ConnectorEvent::DocumentDeleted {
                        document_id: id,
                        occurred_at,
                    },
                    _ => ConnectorEvent::DocumentUpdated {
                        document_id: id,
                        occurred_at,
                    },
                };
                events.push(mapped);
            }
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
                "qb-access",
                "qb-refresh",
                Utc::now() + Duration::hours(1),
                "com.intuit.quickbooks.accounting",
            ))
        }
    }

    fn oauth() -> Arc<dyn OAuth2CodeExchange> {
        Arc::new(FixedOAuth)
    }

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(
            ConnectorKind::QuickBooks,
            AuthKind::OAuth2,
            ScopeId::new_v4(),
        )
        .with_auth_config(serde_json::json!({
            "access_token": "qb-tok",
            "realm_id": "realm123",
            "webhook_verifier_token": "verif",
            "api_base_url": "https://api.test/qb",
        }))
    }

    fn customer(id: &str, updated: &str) -> serde_json::Value {
        serde_json::json!({
            "Id": id, "DisplayName": format!("Cust {id}"),
            "MetaData": { "LastUpdatedTime": updated }
        })
    }

    fn ok_json(value: &serde_json::Value) -> MockResponse {
        MockResponse::ok_json(serde_json::to_vec(value).unwrap())
    }

    fn query_url(where_clause: Option<&str>, start: u32) -> String {
        let mut query = String::from("SELECT * FROM Customer");
        if let Some(w) = where_clause {
            query.push_str(" WHERE ");
            query.push_str(w);
        }
        let _ = write!(
            query,
            " ORDERBY MetaData.LastUpdatedTime ASC STARTPOSITION {start} MAXRESULTS 100"
        );
        format!(
            "https://api.test/qb/v3/company/realm123/query?query={}&minorversion=65",
            percent_encode_path_component(&query)
        )
    }

    #[test]
    fn authenticate_wraps_access_token() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = QuickBooksConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        assert_eq!(
            c.authenticate(&cfg()).unwrap().access_token.expose(),
            "qb-tok"
        );
    }

    #[test]
    fn authenticate_falls_back_to_oauth() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = QuickBooksConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let cfg = ConnectorConfig::new(
            ConnectorKind::QuickBooks,
            AuthKind::OAuth2,
            ScopeId::new_v4(),
        )
        .with_auth_config(serde_json::json!({ "authorization_code": "abc", "realm_id": "r" }));
        assert_eq!(
            c.authenticate(&cfg).unwrap().access_token.expose(),
            "qb-access"
        );
    }

    #[test]
    fn initial_sync_paginates_via_startposition() {
        let transport = Arc::new(MockHttpTransport::new());
        let full: Vec<serde_json::Value> = (0..100)
            .map(|i| customer(&format!("c{i}"), "2024-01-01T00:00:00Z"))
            .collect();
        transport.expect(
            HttpMethod::Get,
            query_url(None, 1),
            ok_json(&serde_json::json!({ "QueryResponse": { "Customer": full } })),
        );
        transport.expect(
            HttpMethod::Get,
            query_url(None, 101),
            ok_json(&serde_json::json!({
                "QueryResponse": { "Customer": [customer("c100", "2024-01-02T00:00:00Z")] }
            })),
        );
        let c = QuickBooksConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 101);
        assert_eq!(transport.recorded().len(), 2);
    }

    #[test]
    fn incremental_sync_adds_where_and_dedupes() {
        let transport = Arc::new(MockHttpTransport::new());
        let prior = "2024-01-01T00:00:00+00:00";
        let where_clause = format!(
            "MetaData.LastUpdatedTime > '{}'",
            "2024-01-01T00:00:00+00:00"
        );
        transport.expect(
            HttpMethod::Get,
            query_url(Some(&where_clause), 1),
            ok_json(&serde_json::json!({ "QueryResponse": { "Customer": [
                customer("old", "2024-01-01T00:00:00Z"),
                customer("new", "2024-02-01T00:00:00Z"),
            ] } })),
        );
        let c = QuickBooksConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let mut state = SyncState::new(c.instance);
        state.cursor = Some(prior.to_string());
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        assert_eq!(res.events.len(), 1);
        assert_eq!(res.events[0].document_id().as_str(), "new");
    }

    #[test]
    fn initial_sync_requires_realm_id() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = QuickBooksConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let cfg = ConnectorConfig::new(
            ConnectorKind::QuickBooks,
            AuthKind::OAuth2,
            ScopeId::new_v4(),
        )
        .with_auth_config(serde_json::json!({ "access_token": "t" }));
        let tok = c.authenticate(&cfg).unwrap();
        assert!(matches!(
            c.initial_sync(&cfg, &tok).unwrap_err(),
            ConnectorError::Sync(_)
        ));
    }

    #[test]
    fn subscribe_webhook_uses_verifier_token_without_http() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = QuickBooksConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let sub = c
            .subscribe_webhook(&cfg(), &tok, "https://hook.example/qb")
            .unwrap();
        assert_eq!(sub.secret.expose(), "verif");
        assert_eq!(sub.provider_subscription_id.as_deref(), Some("realm123"));
        // No HTTP call is made for Intuit webhook registration.
        assert!(transport.recorded().is_empty());
    }

    #[test]
    fn subscribe_webhook_requires_verifier_token() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = QuickBooksConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let cfg = ConnectorConfig::new(
            ConnectorKind::QuickBooks,
            AuthKind::OAuth2,
            ScopeId::new_v4(),
        )
        .with_auth_config(serde_json::json!({ "access_token": "t", "realm_id": "r" }));
        let tok = c.authenticate(&cfg).unwrap();
        assert!(matches!(
            c.subscribe_webhook(&cfg, &tok, "https://hook").unwrap_err(),
            ConnectorError::Webhook(_)
        ));
    }

    #[test]
    fn webhook_emits_every_entity() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = QuickBooksConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let body = serde_json::json!({
            "eventNotifications": [
                {
                    "realmId": "realm123",
                    "dataChangeEvent": {
                        "entities": [
                            { "name": "Customer", "id": "1", "operation": "Create", "lastUpdated": "2024-03-01T00:00:00Z" },
                            { "name": "Customer", "id": "2", "operation": "Update", "lastUpdated": "2024-03-02T00:00:00Z" },
                            { "name": "Customer", "id": "3", "operation": "Delete", "lastUpdated": "2024-03-03T00:00:00Z" }
                        ]
                    }
                }
            ]
        });
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert_eq!(evs.len(), 3);
        assert!(matches!(evs[0], ConnectorEvent::DocumentCreated { .. }));
        assert!(matches!(evs[2], ConnectorEvent::DocumentDeleted { .. }));
    }

    #[test]
    fn fetch_content_renders_summary() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/qb/v3/company/realm123/customer/55?minorversion=65",
            ok_json(&serde_json::json!({
                "Customer": { "Id": "55", "DisplayName": "Acme", "PrimaryEmailAddr": { "Address": "ap@acme" } }
            })),
        );
        let c = QuickBooksConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let fc = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("55"))
            .unwrap();
        let body = String::from_utf8(fc.body).unwrap();
        assert!(body.contains("# Acme"));
        assert!(body.contains("ap@acme"));
    }

    #[test]
    fn fetch_content_maps_401_to_auth_error() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/qb/v3/company/realm123/customer/99?minorversion=65",
            MockResponse::status(401, b"unauthorized".to_vec()),
        );
        let c = QuickBooksConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        assert!(matches!(
            c.fetch_content(&cfg(), &tok, &SourceDocumentId::new("99"))
                .unwrap_err(),
            ConnectorError::Auth(_)
        ));
    }
}
