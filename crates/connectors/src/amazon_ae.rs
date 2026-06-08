//! Amazon.ae connector — Amazon Selling-Partner API (UAE marketplace).
//!
//! * `initial_sync` walks `GET /orders/v0/orders` with the UAE
//!   `MarketplaceIds`, following the `NextToken` cursor until it is
//!   absent.
//! * `incremental_sync` adds the `LastUpdatedAfter` filter keyed off
//!   the stored RFC-3339 watermark; SP-API's filter is inclusive, so
//!   the boundary row is deduped client-side.
//! * `fetch_content` GETs `/orders/v0/orders/{id}` and renders a
//!   Markdown summary.
//! * SP-API notifications are delivered to AWS SQS / EventBridge
//!   destinations rather than an arbitrary callback URL, so
//!   `subscribe_webhook` records a polling-only subscription with no
//!   provider id.
//! * `handle_webhook_event` parses an `OrderChangeNotification`
//!   (single object or batched array).
//!
//! Amazon.ae sells through the Amazon Selling-Partner API, whose
//! `execute-api` calls are signed with AWS Signature Version 4. The
//! short-lived LWA access token is obtained through the injected
//! [`OAuth2CodeExchange`] and rides in the signed `x-amz-access-token`
//! header; the SigV4 `Authorization` is computed from the IAM
//! credentials in `auth_config_json`.

use crate::signing::{sigv4_authorization, SigV4Request};
use chrono::{DateTime, Utc};
use connector_framework::{
    classify_failure, percent_encode_path_component, Connector, ConnectorConfig, ConnectorError,
    ConnectorEvent, ConnectorInstanceId, FetchedContent, HttpRequest, HttpTransport,
    OAuth2CodeExchange, OAuth2Token, Result, SourceDocumentId, SyncRunResult, SyncState,
    WatermarkCursor, WebhookEventTypes, WebhookSecret, WebhookSubscription,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Default Amazon SP-API base URL for the EU region (serves the UAE
/// marketplace).
pub const DEFAULT_API_BASE_URL: &str = "https://sellingpartnerapi-eu.amazon.com";

/// Default AWS region for the EU SP-API endpoint.
pub const DEFAULT_REGION: &str = "eu-west-1";

/// UAE marketplace id.
pub const DEFAULT_MARKETPLACE_ID: &str = "A2VIGQ35RCS4UG";

/// AWS service name for SP-API requests.
const SERVICE: &str = "execute-api";

/// Safety ceiling on the number of `NextToken` pages a single sync
/// will follow.
pub const MAX_PAGES: usize = 100_000;

/// One SP-API order (subset of fields the substrate ingests).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AmazonOrder {
    /// Amazon order id.
    #[serde(rename = "AmazonOrderId", default)]
    pub amazon_order_id: String,
    /// Order status (e.g. `Shipped`, `Canceled`).
    #[serde(rename = "OrderStatus", default)]
    pub order_status: Option<String>,
    /// RFC-3339 purchase timestamp.
    #[serde(rename = "PurchaseDate", default)]
    pub purchase_date: Option<String>,
    /// RFC-3339 last-update timestamp.
    #[serde(rename = "LastUpdateDate", default)]
    pub last_update_date: Option<String>,
}

/// SP-API `getOrders` payload (`{ "payload": { "Orders": [...],
/// "NextToken": ... } }`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AmazonOrdersPayload {
    /// Page of orders.
    #[serde(rename = "Orders", default)]
    pub orders: Vec<AmazonOrder>,
    /// Cursor for the next page, absent on the final page.
    #[serde(rename = "NextToken", default)]
    pub next_token: Option<String>,
}

/// SP-API list-orders response envelope.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AmazonOrdersResponse {
    /// The payload.
    #[serde(default)]
    pub payload: AmazonOrdersPayload,
}

/// SP-API single-order response envelope (`{ "payload": Order }`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AmazonOrderResponse {
    /// The order.
    #[serde(default)]
    pub payload: AmazonOrder,
}

/// SP-API `OrderChangeNotification` (flattened to the fields used).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AmazonWebhookEvent {
    /// Affected Amazon order id.
    #[serde(rename = "AmazonOrderId", default)]
    pub amazon_order_id: String,
    /// Notification type, e.g. `ORDER_CHANGE`.
    #[serde(rename = "NotificationType", default)]
    pub notification_type: String,
}

/// Amazon.ae connector.
pub struct AmazonAeConnector {
    /// Connector instance id.
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    oauth: Arc<dyn OAuth2CodeExchange>,
    api_base_url: String,
    region: String,
    marketplace_id: String,
}

impl std::fmt::Debug for AmazonAeConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AmazonAeConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("region", &self.region)
            .field("marketplace_id", &self.marketplace_id)
            .field("transport", &"<HttpTransport>")
            .field("oauth", &"<OAuth2CodeExchange>")
            .finish()
    }
}

/// IAM credentials resolved from `auth_config_json`.
struct AwsCredentials {
    access_key_id: String,
    secret_access_key: String,
}

/// Everything a single signed SP-API request needs beyond the endpoint
/// and query — bundled so the signing helper stays under the argument
/// limit and the per-request plumbing is passed as one unit.
struct SpApiContext<'a> {
    base_url: &'a str,
    region: &'a str,
    creds: &'a AwsCredentials,
    lwa_token: &'a str,
}

impl AmazonAeConnector {
    /// Construct an Amazon.ae connector.
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
            region: DEFAULT_REGION.to_string(),
            marketplace_id: DEFAULT_MARKETPLACE_ID.to_string(),
        }
    }

    /// Override the SP-API base URL.
    #[must_use]
    pub fn with_api_base_url(mut self, url: impl Into<String>) -> Self {
        self.api_base_url = url.into();
        self
    }

    /// Override the AWS region used for signing.
    #[must_use]
    pub fn with_region(mut self, region: impl Into<String>) -> Self {
        self.region = region.into();
        self
    }

    /// Override the marketplace id.
    #[must_use]
    pub fn with_marketplace_id(mut self, marketplace_id: impl Into<String>) -> Self {
        self.marketplace_id = marketplace_id.into();
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

    fn resolved_region(&self, config: &ConnectorConfig) -> String {
        config
            .auth_config_json
            .get("region")
            .and_then(serde_json::Value::as_str)
            .map_or_else(|| self.region.clone(), std::string::ToString::to_string)
    }

    fn resolved_marketplace_id(&self, config: &ConnectorConfig) -> String {
        config
            .auth_config_json
            .get("marketplace_id")
            .and_then(serde_json::Value::as_str)
            .map_or_else(
                || self.marketplace_id.clone(),
                std::string::ToString::to_string,
            )
    }

    fn credentials(config: &ConnectorConfig) -> Result<AwsCredentials> {
        let access_key_id = config
            .auth_config_json
            .get("aws_access_key_id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ConnectorError::Auth(
                    "amazon_ae: auth_config_json.aws_access_key_id is required".into(),
                )
            })?;
        let secret_access_key = config
            .auth_config_json
            .get("aws_secret_access_key")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ConnectorError::Auth(
                    "amazon_ae: auth_config_json.aws_secret_access_key is required".into(),
                )
            })?;
        Ok(AwsCredentials {
            access_key_id: access_key_id.to_string(),
            secret_access_key: secret_access_key.to_string(),
        })
    }

    /// Host portion (no scheme, no trailing slash) of `base_url`, used
    /// as the SigV4 `Host` header / canonical host.
    fn host_of(base_url: &str) -> &str {
        let no_scheme = base_url
            .strip_prefix("https://")
            .or_else(|| base_url.strip_prefix("http://"))
            .unwrap_or(base_url);
        no_scheme.split('/').next().unwrap_or(no_scheme)
    }

    /// AWS canonical-query-encode a string (RFC-3986 unreserved set
    /// preserved; everything else percent-encoded). Reuses the shared
    /// path-component encoder, which applies the same unreserved set.
    fn query_encode(s: &str) -> String {
        percent_encode_path_component(s)
    }

    /// Build the canonical query string from `pairs`.
    ///
    /// SigV4 requires the query parameters sorted by (encoded) key, then
    /// value. Rather than trusting callers to pre-sort, this sorts
    /// internally as a safety net — mirroring how `sigv4_authorization`
    /// sorts the canonical headers — so adding a query parameter out of
    /// order can never silently invalidate the signature.
    fn canonical_query(pairs: &[(String, String)]) -> String {
        let mut encoded: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| (Self::query_encode(k), Self::query_encode(v)))
            .collect();
        encoded.sort();
        encoded
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&")
    }

    /// Issue a signed SP-API GET against `canonical_uri` + `pairs`
    /// (sorted into canonical order by [`Self::canonical_query`]) and
    /// parse the JSON body.
    fn signed_get<R: DeserializeOwned>(
        &self,
        ctx: &SpApiContext<'_>,
        canonical_uri: &str,
        pairs: &[(String, String)],
        endpoint: &str,
    ) -> Result<R> {
        let host = Self::host_of(ctx.base_url);
        let canonical_query = Self::canonical_query(pairs);
        let amz_date = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
        let authorization = sigv4_authorization(&SigV4Request {
            access_key_id: &ctx.creds.access_key_id,
            secret_access_key: &ctx.creds.secret_access_key,
            region: ctx.region,
            service: SERVICE,
            method: "GET",
            host,
            canonical_uri,
            canonical_query: &canonical_query,
            amz_date: &amz_date,
            payload: b"",
            extra_signed_headers: &[("x-amz-access-token", ctx.lwa_token)],
        });
        let mut url = format!("{}{canonical_uri}", ctx.base_url);
        if !canonical_query.is_empty() {
            url.push('?');
            url.push_str(&canonical_query);
        }
        let req = HttpRequest::get(&url)
            .with_header("Accept", "application/json")
            .with_header("host", host)
            .with_header("x-amz-date", &amz_date)
            .with_header("x-amz-access-token", ctx.lwa_token)
            .with_header("Authorization", &authorization);
        let resp = self.transport.execute(req)?;
        if !resp.is_success() {
            return Err(classify_failure("amazon_ae", endpoint, &resp));
        }
        serde_json::from_slice::<R>(&resp.body).map_err(|e| {
            ConnectorError::Sync(format!(
                "amazon_ae {endpoint} JSON parse failed: {e} (body prefix: {})",
                String::from_utf8_lossy(&resp.body[..resp.body.len().min(256)])
            ))
        })
    }

    /// Walk `getOrders` following `NextToken` until it is absent.
    fn paginate_orders(
        &self,
        ctx: &SpApiContext<'_>,
        marketplace_id: &str,
        last_updated_after: Option<&str>,
    ) -> Result<Vec<AmazonOrder>> {
        let mut out = Vec::<AmazonOrder>::new();
        let mut next_token: Option<String> = None;
        for _ in 0..MAX_PAGES {
            // SP-API requires only `NextToken` + `MarketplaceIds` once a
            // cursor is in hand; on the first page it takes the filter
            // params. Pairs are emitted sorted by key (canonical order).
            let mut pairs: Vec<(String, String)> = Vec::new();
            if let Some(token) = &next_token {
                pairs.push(("MarketplaceIds".to_string(), marketplace_id.to_string()));
                pairs.push(("NextToken".to_string(), token.clone()));
            } else {
                if let Some(after) = last_updated_after {
                    pairs.push(("LastUpdatedAfter".to_string(), after.to_string()));
                }
                pairs.push(("MarketplaceIds".to_string(), marketplace_id.to_string()));
            }
            let resp: AmazonOrdersResponse =
                self.signed_get(ctx, "/orders/v0/orders", &pairs, "/orders/v0/orders")?;
            out.extend(resp.payload.orders);
            match resp.payload.next_token {
                Some(t) if !t.is_empty() => next_token = Some(t),
                _ => return Ok(out),
            }
        }
        Err(ConnectorError::Sync(format!(
            "amazon_ae /orders/v0/orders exceeded {MAX_PAGES} pages"
        )))
    }
}

fn parse_rfc3339(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

fn order_watermark(o: &AmazonOrder) -> Option<DateTime<Utc>> {
    o.last_update_date
        .as_deref()
        .and_then(parse_rfc3339)
        .or_else(|| o.purchase_date.as_deref().and_then(parse_rfc3339))
}

impl Connector for AmazonAeConnector {
    fn authenticate(&self, config: &ConnectorConfig) -> Result<OAuth2Token> {
        // The IAM credentials must be present to sign requests.
        Self::credentials(config)?;
        // The LWA access token is obtained by exchanging the stored
        // refresh grant / authorization code through the OAuth2 hook.
        let auth_code = config
            .auth_config_json
            .get("authorization_code")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ConnectorError::Auth(
                    "amazon_ae authenticate: auth_config_json.authorization_code is required"
                        .into(),
                )
            })?;
        let token = self.oauth.exchange_code(config, auth_code)?;
        if token.access_token.expose().is_empty() {
            return Err(ConnectorError::Auth(
                "amazon_ae authenticate: OAuth2 exchange returned an empty access token".into(),
            ));
        }
        Ok(token)
    }

    fn initial_sync(&self, config: &ConnectorConfig, token: &OAuth2Token) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        let region = self.resolved_region(config);
        let marketplace_id = self.resolved_marketplace_id(config);
        let creds = Self::credentials(config)?;
        let ctx = SpApiContext {
            base_url: &base_url,
            region: &region,
            creds: &creds,
            lwa_token: token.access_token.expose(),
        };
        let orders = self.paginate_orders(&ctx, &marketplace_id, None)?;
        let mut events = Vec::with_capacity(orders.len());
        let mut cursor = WatermarkCursor::empty();
        for o in &orders {
            let occurred_at = order_watermark(o).unwrap_or_else(Utc::now);
            events.push(ConnectorEvent::DocumentCreated {
                document_id: SourceDocumentId::new(o.amazon_order_id.clone()),
                occurred_at,
            });
            if let Some(t) = order_watermark(o) {
                cursor.observe(t, &o.amazon_order_id);
            }
        }
        Ok(SyncRunResult {
            events,
            next_cursor: cursor.to_cursor_string(),
        })
    }

    fn incremental_sync(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        state: &SyncState,
    ) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        let region = self.resolved_region(config);
        let marketplace_id = self.resolved_marketplace_id(config);
        let creds = Self::credentials(config)?;
        let ctx = SpApiContext {
            base_url: &base_url,
            region: &region,
            creds: &creds,
            lwa_token: token.access_token.expose(),
        };
        let prior = WatermarkCursor::parse(state.cursor.as_deref());
        let since = prior.query_since();
        let orders = self.paginate_orders(&ctx, &marketplace_id, since.as_deref())?;
        let mut events = Vec::new();
        let mut cursor = prior.clone();
        for o in &orders {
            let Some(updated) = order_watermark(o) else {
                continue;
            };
            if !prior.should_emit(updated, &o.amazon_order_id) {
                continue;
            }
            events.push(ConnectorEvent::DocumentUpdated {
                document_id: SourceDocumentId::new(o.amazon_order_id.clone()),
                occurred_at: updated,
            });
            cursor.observe(updated, &o.amazon_order_id);
        }
        Ok(SyncRunResult {
            events,
            next_cursor: cursor.to_cursor_string(),
        })
    }

    fn fetch_content(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        document_id: &SourceDocumentId,
    ) -> Result<FetchedContent> {
        let base_url = self.resolved_base_url(config);
        let region = self.resolved_region(config);
        let creds = Self::credentials(config)?;
        let ctx = SpApiContext {
            base_url: &base_url,
            region: &region,
            creds: &creds,
            lwa_token: token.access_token.expose(),
        };
        let id = document_id.as_str();
        let id_enc = percent_encode_path_component(id);
        let canonical_uri = format!("/orders/v0/orders/{id_enc}");
        let resp: AmazonOrderResponse =
            self.signed_get(&ctx, &canonical_uri, &[], "/orders/v0/orders/{id}")?;
        let order = resp.payload;
        let title = format!("Order {id}");
        let mut md = String::new();
        md.push_str("# ");
        md.push_str(&title);
        md.push_str("\n\n");
        if let Some(status) = order.order_status.as_deref().filter(|s| !s.is_empty()) {
            md.push_str("**Status:** ");
            md.push_str(status);
            md.push_str("\n\n");
        }
        if let Some(purchase) = order.purchase_date.as_deref().filter(|s| !s.is_empty()) {
            md.push_str("**Purchased:** ");
            md.push_str(purchase);
            md.push_str("\n\n");
        }
        let body = md.trim_end().to_string();
        Ok(FetchedContent::text(body, "text/markdown")
            .with_title(title)
            .with_metadata(serde_json::json!({
                "provider": "amazon_ae",
                "order_id": id,
                "status": order.order_status,
                "last_update_date": order.last_update_date,
            }))
            .with_source_url(format!("{base_url}/orders/v0/orders/{id}")))
    }

    fn subscribe_webhook(
        &self,
        config: &ConnectorConfig,
        _token: &OAuth2Token,
        callback_url: &str,
    ) -> Result<WebhookSubscription> {
        // SP-API notifications are delivered to AWS SQS / EventBridge
        // destinations, not an arbitrary HTTPS callback — there is no
        // way to register `callback_url` directly. Record a polling-only
        // subscription so the runtime falls back to incremental_sync.
        Ok(WebhookSubscription::new(
            self.instance,
            callback_url,
            WebhookSecret::new(
                config
                    .auth_config_json
                    .get("webhook_secret")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("amazon-ae-webhook-secret"),
            ),
            WebhookEventTypes::all(),
            None,
        ))
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        let deliveries: Vec<AmazonWebhookEvent> =
            if let Ok(batch) = serde_json::from_slice::<Vec<AmazonWebhookEvent>>(body) {
                batch
            } else {
                vec![serde_json::from_slice::<AmazonWebhookEvent>(body)?]
            };
        if deliveries.is_empty() {
            return Err(ConnectorError::Webhook(
                "empty amazon_ae webhook batch".into(),
            ));
        }
        let mut events = Vec::with_capacity(deliveries.len());
        for delivery in deliveries {
            if delivery.amazon_order_id.is_empty() {
                return Err(ConnectorError::Webhook(
                    "amazon_ae webhook event missing AmazonOrderId".into(),
                ));
            }
            let occurred_at = Utc::now();
            events.push(ConnectorEvent::DocumentUpdated {
                document_id: SourceDocumentId::new(delivery.amazon_order_id),
                occurred_at,
            });
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
                "lwa-access",
                "lwa-refresh",
                Utc::now() + Duration::hours(1),
                "read",
            ))
        }
    }

    fn oauth() -> Arc<dyn OAuth2CodeExchange> {
        Arc::new(FixedOAuth)
    }

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(ConnectorKind::AmazonAE, AuthKind::OAuth2, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({
                "authorization_code": "code123",
                "aws_access_key_id": "AKIDEXAMPLE",
                "aws_secret_access_key": "secret",
                "api_base_url": "https://api.test",
                "region": "eu-west-1",
                "marketplace_id": "A2VIGQ35RCS4UG",
            }))
    }

    fn ok_json(value: &serde_json::Value) -> MockResponse {
        MockResponse::ok_json(serde_json::to_vec(value).unwrap())
    }

    fn orders_url(query_pairs: &[(&str, &str)]) -> String {
        let q = query_pairs
            .iter()
            .map(|(k, v)| {
                format!(
                    "{}={}",
                    percent_encode_path_component(k),
                    percent_encode_path_component(v)
                )
            })
            .collect::<Vec<_>>()
            .join("&");
        format!("https://api.test/orders/v0/orders?{q}")
    }

    #[test]
    fn authenticate_requires_credentials_and_code() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = AmazonAeConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        assert_eq!(
            c.authenticate(&cfg()).unwrap().access_token.expose(),
            "lwa-access"
        );
        let no_creds =
            ConnectorConfig::new(ConnectorKind::AmazonAE, AuthKind::OAuth2, ScopeId::new_v4())
                .with_auth_config(serde_json::json!({ "authorization_code": "x" }));
        assert!(matches!(
            c.authenticate(&no_creds),
            Err(ConnectorError::Auth(_))
        ));
    }

    #[test]
    fn initial_sync_follows_next_token_and_signs() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            orders_url(&[("MarketplaceIds", "A2VIGQ35RCS4UG")]),
            ok_json(&serde_json::json!({"payload": {
                "Orders": [
                    {"AmazonOrderId": "1", "LastUpdateDate": "2024-01-01T00:00:00Z"},
                    {"AmazonOrderId": "2", "LastUpdateDate": "2024-01-02T00:00:00Z"}
                ],
                "NextToken": "tok2"
            }})),
        );
        transport.expect(
            HttpMethod::Get,
            orders_url(&[("MarketplaceIds", "A2VIGQ35RCS4UG"), ("NextToken", "tok2")]),
            ok_json(&serde_json::json!({"payload": {
                "Orders": [
                    {"AmazonOrderId": "3", "LastUpdateDate": "2024-01-03T00:00:00Z"}
                ]
            }})),
        );
        let c = AmazonAeConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 3);
        assert_eq!(
            res.next_cursor.as_deref(),
            Some("2024-01-03T00:00:00+00:00|3")
        );
        let recorded = transport.recorded();
        let headers = &recorded[0].headers;
        assert!(headers
            .iter()
            .any(|(k, v)| k == "x-amz-access-token" && v == "lwa-access"));
        assert!(headers
            .iter()
            .any(|(k, v)| k == "Authorization" && v.starts_with("AWS4-HMAC-SHA256 ")));
    }

    #[test]
    fn incremental_sync_dedups_boundary() {
        let transport = Arc::new(MockHttpTransport::new());
        let since = "2024-03-01T00:00:00+00:00";
        transport.expect(
            HttpMethod::Get,
            orders_url(&[
                ("LastUpdatedAfter", since),
                ("MarketplaceIds", "A2VIGQ35RCS4UG"),
            ]),
            ok_json(&serde_json::json!({"payload": {
                "Orders": [
                    {"AmazonOrderId": "10", "LastUpdateDate": "2024-03-01T00:00:00Z"},
                    {"AmazonOrderId": "13", "LastUpdateDate": "2024-03-01T00:00:00Z"},
                    {"AmazonOrderId": "11", "LastUpdateDate": "2024-06-01T00:00:00Z"}
                ]
            }})),
        );
        let c = AmazonAeConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let mut state = SyncState::new(c.instance);
        // Prior run already emitted `10` at the boundary instant; the cursor
        // records it. This run re-queries the instant inclusively and must NOT
        // re-emit `10`, still surface the brand-new `13` at the same second,
        // and advance past the later row.
        state.cursor = Some(format!("{since}|10"));
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        let ids: Vec<&str> = res
            .events
            .iter()
            .map(|e| match e {
                ConnectorEvent::DocumentUpdated { document_id, .. } => document_id.as_str(),
                other => panic!("unexpected event: {other:?}"),
            })
            .collect();
        assert_eq!(ids, ["13", "11"]);
        assert_eq!(
            res.next_cursor.as_deref(),
            Some("2024-06-01T00:00:00+00:00|11")
        );
    }

    #[test]
    fn fetch_content_assembles_markdown() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/orders/v0/orders/55".to_string(),
            ok_json(&serde_json::json!({"payload": {
                "AmazonOrderId": "55",
                "OrderStatus": "Shipped",
                "PurchaseDate": "2024-01-01T00:00:00Z"
            }})),
        );
        let c = AmazonAeConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let fc = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("55"))
            .unwrap();
        let body = String::from_utf8(fc.body).unwrap();
        assert!(body.contains("# Order 55"));
        assert!(body.contains("**Status:** Shipped"));
    }

    #[test]
    fn subscribe_webhook_is_polling_only() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = AmazonAeConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let sub = c
            .subscribe_webhook(&cfg(), &tok, "https://hook.example/amazon")
            .unwrap();
        assert!(sub.provider_subscription_id.is_none());
        assert!(transport.recorded().is_empty());
    }

    #[test]
    fn webhook_parses_single_and_batch() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = AmazonAeConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let single = c
            .handle_webhook_event(br#"{"AmazonOrderId": "7", "NotificationType": "ORDER_CHANGE"}"#)
            .unwrap();
        assert!(matches!(single[0], ConnectorEvent::DocumentUpdated { .. }));
        let batch = c
            .handle_webhook_event(
                br#"[{"AmazonOrderId": "8", "NotificationType": "ORDER_CHANGE"}, {"AmazonOrderId": "9", "NotificationType": "ORDER_CHANGE"}]"#,
            )
            .unwrap();
        assert_eq!(batch.len(), 2);
    }
}
