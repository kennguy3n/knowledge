//! MangoPay connector — Mangopay REST API (`https://api.mangopay.com/v2.01`).
//!
//! Mangopay — French payment API (wallets, pay-ins/payouts).
//!
//! ## Authentication (OAuth 2.0 `client_credentials`)
//!
//! Mangopay does **not** accept a static API key in a custom header.
//! Every call is authenticated with a short-lived **Bearer token**
//! obtained from `POST /v2.01/oauth/token` using HTTP Basic access
//! authentication — `Authorization: Basic base64("{ClientId}:{ApiKey}")`
//! and a form body of `grant_type=client_credentials`. The response
//! carries `{access_token, token_type, expires_in}`; the connector
//! then sends `Authorization: Bearer <access_token>` on resource calls.
//! `authenticate` reads `client_id` + `api_key` from `auth_config_json`
//! and performs this exchange, falling back to the injected
//! [`OAuth2CodeExchange`] when an `authorization_code` grant is
//! configured instead. (See <https://docs.mangopay.com/api-reference/overview/authentication>.)
//!
//! ## Resource model (wallet-scoped transactions)
//!
//! Mangopay has **no global pay-in collection**; pay-ins are read as
//! the `PAYIN`-typed entries of a wallet's transaction list. So:
//!
//! * `initial_sync` / `incremental_sync` page
//!   `GET /v2.01/{ClientId}/wallets/{WalletId}/transactions`
//!   (`page` / `per_page`, `Sort=CreationDate:ASC`, `Type=PAYIN`),
//!   following the `x-number-of-pages` response header and tracking
//!   the maximum `CreationDate` (a Unix timestamp) as the watermark.
//!   Incremental runs add `AfterDate=<unix>` and dedup the inclusive
//!   boundary row. `client_id` and `wallet_id` are required.
//! * `fetch_content` GETs a single pay-in
//!   (`GET /v2.01/{ClientId}/payins/{PayInId}`).
//! * Mangopay webhooks are configured in the provider dashboard and
//!   delivered as `EventType` / `RessourceId` / `Date` parameters, so
//!   `subscribe_webhook` records a polling-only subscription and
//!   `handle_webhook_event` parses the delivered notification.

use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use connector_framework::{
    classify_failure, percent_encode_path_component, Connector, ConnectorConfig, ConnectorError,
    ConnectorEvent, ConnectorInstanceId, FetchedContent, HttpRequest, HttpTransport,
    OAuth2CodeExchange, OAuth2Token, Result, SourceDocumentId, SyncRunResult, SyncState,
    WebhookEventTypes, WebhookSecret, WebhookSubscription,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// Default Mangopay API base URL (already carries the `/v2.01` version).
pub const DEFAULT_API_BASE_URL: &str = "https://api.mangopay.com/v2.01";
/// Default scope recorded on the issued bearer token.
pub const DEFAULT_SCOPE: &str = "payins";
/// Page size for the wallet transaction listing (`per_page`,
/// Mangopay caps this at 100).
pub const DEFAULT_PAGE_SIZE: u32 = 100;
/// Safety ceiling on the number of pages a single sync walks.
pub const MAX_PAGES: usize = 100_000;

/// OAuth `client_credentials` token response from `POST /oauth/token`.
#[derive(Debug, Clone, Default, Deserialize)]
struct MangoPayTokenResponse {
    #[serde(default)]
    access_token: String,
    #[serde(default)]
    token_type: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
}

/// A wallet transaction (`Type` is one of `PAYIN` / `PAYOUT` /
/// `TRANSFER` / `CONVERSION`). Mangopay uses PascalCase field names
/// and Unix-timestamp dates.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct MangoPayRecord {
    #[serde(rename = "Id", default)]
    id: String,
    #[serde(rename = "Status", default)]
    status: Option<String>,
    #[serde(rename = "Type", default)]
    kind: Option<String>,
    #[serde(rename = "Tag", default)]
    tag: Option<String>,
    #[serde(rename = "CreationDate", default)]
    creation_date: Option<i64>,
}

/// Mangopay webhook notification (`EventType` / `RessourceId` / `Date`).
/// `RessourceId` is Mangopay's documented (misspelled) field carrying
/// the `Id` of the object the event occurred on.
#[derive(Debug, Clone, Default, Deserialize)]
struct MangoPayWebhookEvent {
    #[serde(
        rename = "RessourceId",
        alias = "ResourceId",
        alias = "resource_id",
        default
    )]
    resource_id: serde_json::Value,
    #[serde(rename = "EventType", alias = "event_type", default)]
    event_type: String,
}

/// MangoPay connector.
#[derive(Clone)]
pub struct MangoPayConnector {
    /// Connector instance id.
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    oauth: Arc<dyn OAuth2CodeExchange>,
    api_base_url: String,
    page_size: u32,
}

impl std::fmt::Debug for MangoPayConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MangoPayConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("page_size", &self.page_size)
            .field("transport", &"<HttpTransport>")
            .field("oauth", &"<OAuth2CodeExchange>")
            .finish()
    }
}

impl MangoPayConnector {
    /// Construct a MangoPay connector.
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

    /// Override the MangoPay API base URL.
    #[must_use]
    pub fn with_api_base_url(mut self, url: impl Into<String>) -> Self {
        self.api_base_url = url.into();
        self
    }

    /// Override the page size.
    #[must_use]
    pub fn with_page_size(mut self, page_size: u32) -> Self {
        self.page_size = page_size.clamp(1, 100);
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

    /// Exchange `{ClientId}:{ApiKey}` for a short-lived Bearer token via
    /// `POST /oauth/token` (HTTP Basic + `grant_type=client_credentials`).
    fn exchange_client_credentials(
        &self,
        base_url: &str,
        client_id: &str,
        api_key: &str,
    ) -> Result<OAuth2Token> {
        let basic = base64_encode(format!("{client_id}:{api_key}").as_bytes());
        let req = HttpRequest::post(
            format!("{base_url}/oauth/token"),
            b"grant_type=client_credentials".to_vec(),
        )
        .with_header("Authorization", format!("Basic {basic}"))
        .with_header("Content-Type", "application/x-www-form-urlencoded")
        .with_header("Accept", "application/json");
        let resp = self.transport.execute(req)?;
        if !resp.is_success() {
            return Err(classify_failure("mangopay", "/oauth/token", &resp));
        }
        let parsed: MangoPayTokenResponse = serde_json::from_slice(&resp.body).map_err(|e| {
            ConnectorError::Auth(format!("mangopay /oauth/token parse failed: {e}"))
        })?;
        if parsed.access_token.is_empty() {
            return Err(ConnectorError::Auth(
                "mangopay /oauth/token returned an empty access_token".into(),
            ));
        }
        let ttl = parsed.expires_in.filter(|s| *s > 0).unwrap_or(3600);
        let mut token = OAuth2Token::new_without_refresh(
            parsed.access_token,
            Utc::now() + Duration::seconds(ttl),
            DEFAULT_SCOPE,
        );
        // Normalise the scheme: Mangopay returns "Bearer", but be
        // defensive about casing so the request header is canonical.
        token.token_type = match parsed.token_type.as_deref() {
            Some(t) if t.eq_ignore_ascii_case("bearer") || t.is_empty() => "Bearer".to_string(),
            Some(other) => other.to_string(),
            None => "Bearer".to_string(),
        };
        Ok(token)
    }

    fn http_get<R: DeserializeOwned>(
        &self,
        endpoint: &str,
        url: &str,
        token: &OAuth2Token,
    ) -> Result<R> {
        let (parsed, _pages) = self.http_get_paged(endpoint, url, token)?;
        Ok(parsed)
    }

    /// GET `url`, returning the parsed body plus the total page count
    /// from Mangopay's `x-number-of-pages` response header (used to
    /// terminate pagination).
    fn http_get_paged<R: DeserializeOwned>(
        &self,
        endpoint: &str,
        url: &str,
        token: &OAuth2Token,
    ) -> Result<(R, Option<u32>)> {
        let req = apply_auth(
            HttpRequest::get(url).with_header("Accept", "application/json"),
            token,
        );
        let resp = self.transport.execute(req)?;
        if !resp.is_success() {
            return Err(classify_failure("mangopay", endpoint, &resp));
        }
        // Match the header name case-insensitively: HTTP header names are
        // case-insensitive (RFC 7230 §3.2) and we must not depend on the
        // transport lower-casing them.
        let total_pages = resp
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("x-number-of-pages"))
            .and_then(|(_, v)| v.trim().parse::<u32>().ok());
        let parsed = serde_json::from_slice::<R>(&resp.body).map_err(|e| {
            ConnectorError::Sync(format!(
                "mangopay {endpoint} JSON parse failed: {e} (body prefix: {})",
                String::from_utf8_lossy(&resp.body[..resp.body.len().min(256)])
            ))
        })?;
        Ok((parsed, total_pages))
    }

    fn paginate_records(
        &self,
        base_url: &str,
        client_id: &str,
        wallet_id: &str,
        token: &OAuth2Token,
        after_date: Option<i64>,
    ) -> Result<Vec<MangoPayRecord>> {
        let cid = percent_encode_path_component(client_id);
        let wid = percent_encode_path_component(wallet_id);
        let mut records = Vec::<MangoPayRecord>::new();
        for page in 1..=MAX_PAGES {
            let mut url = format!(
                "{base_url}/{cid}/wallets/{wid}/transactions?page={page}&per_page={}&Sort=CreationDate:ASC&Type=PAYIN",
                self.page_size
            );
            if let Some(after) = after_date {
                url.push_str("&AfterDate=");
                url.push_str(&after.to_string());
            }
            let (page_records, total_pages): (Vec<MangoPayRecord>, Option<u32>) =
                self.http_get_paged("/wallets/{id}/transactions", &url, token)?;
            let count = page_records.len();
            records.extend(page_records);
            if let Some(total) = total_pages {
                // Authoritative terminator: stop once we've read the
                // last page Mangopay reported.
                if page >= total.max(1) as usize {
                    return Ok(records);
                }
            } else if count < self.page_size as usize {
                // Fallback when the header is absent: a short page is
                // the last one.
                return Ok(records);
            }
        }
        Err(ConnectorError::Sync(format!(
            "mangopay wallet transactions exceeded {MAX_PAGES} pages"
        )))
    }
}

/// Attach `Authorization: Bearer <token>` (Mangopay authenticates every
/// resource call with the OAuth bearer token minted by `authenticate`;
/// the API-key is only ever used to obtain that token).
fn apply_auth(req: HttpRequest, token: &OAuth2Token) -> HttpRequest {
    let scheme = if token.token_type.is_empty() {
        "Bearer"
    } else {
        token.token_type.as_str()
    };
    req.with_header(
        "Authorization",
        format!("{scheme} {}", token.access_token.expose()),
    )
}

/// Standard Base64 (with padding) of `input`. Inlined per the codebase
/// convention of not pulling a `base64` dependency for a tiny encoder
/// (see `content::decode_base64`).
fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((n >> 18) & 0x3f) as usize] as char);
        out.push(TABLE[((n >> 12) & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[((n >> 6) & 0x3f) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(n & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

fn config_str<'a>(config: &'a ConnectorConfig, key: &str) -> Option<&'a str> {
    config
        .auth_config_json
        .get(key)
        .and_then(serde_json::Value::as_str)
}

/// Resolve the `client_id` / `wallet_id` pair required to address a
/// wallet's transactions, failing fast with a clear error otherwise.
fn require_ids(config: &ConnectorConfig) -> Result<(String, String)> {
    let client_id = config_str(config, "client_id").ok_or_else(|| {
        ConnectorError::Sync("mangopay: auth_config_json.client_id is required".into())
    })?;
    let wallet_id = config_str(config, "wallet_id").ok_or_else(|| {
        ConnectorError::Sync("mangopay: auth_config_json.wallet_id is required".into())
    })?;
    Ok((client_id.to_string(), wallet_id.to_string()))
}

fn watermark_from(record: &MangoPayRecord) -> Option<DateTime<Utc>> {
    record
        .creation_date
        .and_then(|secs| DateTime::from_timestamp(secs, 0))
}

fn id_value_to_string(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) if !s.is_empty() => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

impl Connector for MangoPayConnector {
    fn authenticate(&self, config: &ConnectorConfig) -> Result<OAuth2Token> {
        let base_url = self.resolved_base_url(config);
        if let Some(api_key) = config_str(config, "api_key") {
            let client_id = config_str(config, "client_id").ok_or_else(|| {
                ConnectorError::Auth(
                    "mangopay authenticate: auth_config_json.client_id is required alongside api_key"
                        .into(),
                )
            })?;
            return self.exchange_client_credentials(&base_url, client_id, api_key);
        }
        let auth_code = config_str(config, "authorization_code").ok_or_else(|| {
            ConnectorError::Auth(
                "mangopay authenticate: auth_config_json.api_key (+ client_id) or .authorization_code is required"
                    .into(),
            )
        })?;
        self.oauth.exchange_code(config, auth_code)
    }

    fn initial_sync(&self, config: &ConnectorConfig, token: &OAuth2Token) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        let (client_id, wallet_id) = require_ids(config)?;
        let records = self.paginate_records(&base_url, &client_id, &wallet_id, token, None)?;
        let mut events = Vec::with_capacity(records.len());
        let mut watermark: Option<DateTime<Utc>> = None;
        for record in &records {
            let occurred_at = watermark_from(record).unwrap_or_else(Utc::now);
            events.push(ConnectorEvent::DocumentCreated {
                document_id: SourceDocumentId::new(record.id.clone()),
                occurred_at,
            });
            if let Some(t) = watermark_from(record) {
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
        let (client_id, wallet_id) = require_ids(config)?;
        let prior: Option<DateTime<Utc>> = state
            .cursor
            .as_deref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc));
        // Mangopay's `AfterDate` filters on `CreationDate` (Unix
        // seconds); the boundary is inclusive so we still dedup below.
        let after = prior.map(|p| p.timestamp());
        let records = self.paginate_records(&base_url, &client_id, &wallet_id, token, after)?;
        let mut events = Vec::new();
        let mut watermark = prior;
        for record in &records {
            let Some(updated) = watermark_from(record) else {
                continue;
            };
            if prior.is_some_and(|p| updated <= p) {
                continue;
            }
            events.push(ConnectorEvent::DocumentUpdated {
                document_id: SourceDocumentId::new(record.id.clone()),
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
        let base_url = self.resolved_base_url(config);
        let (client_id, _wallet_id) = require_ids(config)?;
        let cid = percent_encode_path_component(&client_id);
        let id = document_id.as_str();
        let id_enc = percent_encode_path_component(id);
        let url = format!("{base_url}/{cid}/payins/{id_enc}");
        let record: MangoPayRecord = self.http_get("/payins/{id}", &url, token)?;
        let status = record.status.as_deref().unwrap_or("unknown");
        let tag = record.tag.as_deref().unwrap_or("(no tag)");
        let kind = record.kind.as_deref().unwrap_or("PAYIN");
        let body =
            format!("# Mangopay pay-in {id}\n\nType: {kind}\nStatus: {status}\nTag: {tag}\n");
        Ok(FetchedContent::text(body, "text/markdown")
            .with_title(format!("Mangopay pay-in {id}"))
            .with_metadata(serde_json::json!({
                "provider": "mangopay",
                "record_id": record.id,
                "status": record.status,
                "type": record.kind,
                "creation_date": record.creation_date,
            })))
    }

    fn subscribe_webhook(
        &self,
        config: &ConnectorConfig,
        _token: &OAuth2Token,
        callback_url: &str,
    ) -> Result<WebhookSubscription> {
        // Mangopay webhooks ("Hooks") are registered in the provider
        // dashboard; record a polling-only subscription so the runtime
        // falls back to incremental_sync.
        let secret = config_str(config, "webhook_secret").unwrap_or("mangopay-webhook-secret");
        Ok(WebhookSubscription::new(
            self.instance,
            callback_url,
            WebhookSecret::new(secret),
            WebhookEventTypes::all(),
            None,
        ))
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        let deliveries: Vec<MangoPayWebhookEvent> =
            if let Ok(batch) = serde_json::from_slice::<Vec<MangoPayWebhookEvent>>(body) {
                batch
            } else {
                vec![
                    serde_json::from_slice::<MangoPayWebhookEvent>(body).map_err(|e| {
                        ConnectorError::Webhook(format!("mangopay webhook parse failed: {e}"))
                    })?,
                ]
            };
        if deliveries.is_empty() {
            return Err(ConnectorError::Webhook(
                "empty mangopay webhook batch".into(),
            ));
        }
        let mut events = Vec::with_capacity(deliveries.len());
        for delivery in deliveries {
            let id_str = id_value_to_string(&delivery.resource_id).ok_or_else(|| {
                ConnectorError::Webhook("mangopay webhook event missing RessourceId".into())
            })?;
            let id = SourceDocumentId::new(id_str);
            let occurred_at = Utc::now();
            // Mangopay EventTypes look like `PAYIN_NORMAL_CREATED` /
            // `PAYIN_NORMAL_SUCCEEDED` / `PAYIN_NORMAL_FAILED`. A pay-in
            // is never deleted, so everything that is not a creation is
            // a state change.
            let event = if delivery.event_type.contains("CREATED") {
                ConnectorEvent::DocumentCreated {
                    document_id: id,
                    occurred_at,
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
                "payins",
            ))
        }
    }

    fn oauth() -> Arc<dyn OAuth2CodeExchange> {
        Arc::new(FixedOAuth)
    }

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(ConnectorKind::MangoPay, AuthKind::ApiKey, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({
                "api_key": "mangopay-key",
                "client_id": "client-1",
                "wallet_id": "wallet-1",
                "api_base_url": "https://api.test/mangopay",
                "webhook_secret": "mangopay-secret",
            }))
    }

    fn cfg_oauth() -> ConnectorConfig {
        ConnectorConfig::new(ConnectorKind::MangoPay, AuthKind::OAuth2, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({
                "authorization_code": "auth-code",
                "client_id": "client-1",
                "wallet_id": "wallet-1",
                "api_base_url": "https://api.test/mangopay",
                "webhook_secret": "mangopay-secret",
            }))
    }

    fn ok_json(value: &serde_json::Value) -> MockResponse {
        MockResponse::ok_json(serde_json::to_vec(value).unwrap())
    }

    /// 200 OK whose body is a JSON array plus an `x-number-of-pages`
    /// header (Mangopay's authoritative pagination terminator).
    fn page_json(value: &serde_json::Value, total_pages: u32) -> MockResponse {
        MockResponse {
            status: 200,
            headers: vec![
                ("content-type".into(), "application/json".into()),
                ("x-number-of-pages".into(), total_pages.to_string()),
            ],
            body: serde_json::to_vec(value).unwrap(),
        }
    }

    /// Register the `POST /oauth/token` client-credentials exchange.
    fn expect_token(transport: &MockHttpTransport, base: &str) {
        transport.expect(
            HttpMethod::Post,
            format!("{base}/oauth/token"),
            ok_json(&serde_json::json!({
                "access_token": "mp-bearer",
                "token_type": "Bearer",
                "expires_in": 3600
            })),
        );
    }

    #[test]
    fn authenticate_exchanges_client_credentials_for_bearer() {
        let transport = Arc::new(MockHttpTransport::new());
        expect_token(&transport, "https://api.test/mangopay");
        let c = MangoPayConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let token = c.authenticate(&cfg()).unwrap();
        assert_eq!(token.access_token.expose(), "mp-bearer");
        assert_eq!(token.token_type, "Bearer");
        let recorded = transport.recorded();
        // Basic auth over base64("client-1:mangopay-key") + form grant.
        let expected_basic = format!("Basic {}", base64_encode(b"client-1:mangopay-key"));
        assert!(recorded[0]
            .headers
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("authorization") && *v == expected_basic));
        assert_eq!(recorded[0].body, b"grant_type=client_credentials");
    }

    #[test]
    fn base64_encode_matches_rfc4648_vectors() {
        // RFC 4648 §10 test vectors — exercises all three tail cases
        // (0/1/2 leftover bytes → "", one '=', two '='), which is where
        // a hand-rolled encoder is most likely to be wrong.
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        // High-bit bytes exercise the `+` / `/` alphabet entries.
        assert_eq!(base64_encode(&[0xfb, 0xff, 0xfe]), "+//+");
    }

    #[test]
    fn authenticate_falls_back_to_oauth_code() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = MangoPayConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let token = c.authenticate(&cfg_oauth()).unwrap();
        assert_eq!(token.access_token.expose(), "unused");
        assert_eq!(token.token_type, "Bearer");
    }

    #[test]
    fn authenticate_requires_credential() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = MangoPayConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let bare =
            ConnectorConfig::new(ConnectorKind::MangoPay, AuthKind::ApiKey, ScopeId::new_v4());
        assert!(matches!(
            c.authenticate(&bare),
            Err(ConnectorError::Auth(_))
        ));
    }

    #[test]
    fn authenticate_api_key_requires_client_id() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = MangoPayConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let cfg =
            ConnectorConfig::new(ConnectorKind::MangoPay, AuthKind::ApiKey, ScopeId::new_v4())
                .with_auth_config(serde_json::json!({ "api_key": "k" }));
        assert!(matches!(c.authenticate(&cfg), Err(ConnectorError::Auth(_))));
    }

    #[test]
    fn initial_sync_lists_wallet_payins_and_sends_bearer() {
        let transport = Arc::new(MockHttpTransport::new());
        expect_token(&transport, "https://api.test/mangopay");
        transport.expect(
            HttpMethod::Get,
            "https://api.test/mangopay/client-1/wallets/wallet-1/transactions?page=1&per_page=2&Sort=CreationDate:ASC&Type=PAYIN",
            page_json(
                &serde_json::json!([
                    {"Id": "o-1", "Type": "PAYIN", "Status": "SUCCEEDED", "CreationDate": 1_704_067_200_i64},
                    {"Id": "o-2", "Type": "PAYIN", "Status": "SUCCEEDED", "CreationDate": 1_704_153_600_i64}
                ]),
                2,
            ),
        );
        transport.expect(
            HttpMethod::Get,
            "https://api.test/mangopay/client-1/wallets/wallet-1/transactions?page=2&per_page=2&Sort=CreationDate:ASC&Type=PAYIN",
            page_json(
                &serde_json::json!([
                    {"Id": "o-3", "Type": "PAYIN", "Status": "SUCCEEDED", "CreationDate": 1_704_240_000_i64}
                ]),
                2,
            ),
        );
        let c = MangoPayConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth())
            .with_page_size(2);
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 3);
        assert_eq!(
            res.next_cursor.as_deref(),
            Some("2024-01-03T00:00:00+00:00")
        );
        let recorded = transport.recorded();
        // recorded[0] = token POST; recorded[1] = first transactions GET.
        assert!(recorded[1]
            .headers
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("authorization") && v == "Bearer mp-bearer"));
        assert!(!recorded[1]
            .headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("X-MangoPay-Api-Key")));
    }

    #[test]
    fn incremental_sync_filters_after_date_and_dedups() {
        let transport = Arc::new(MockHttpTransport::new());
        expect_token(&transport, "https://api.test/mangopay");
        // 2024-03-01T00:00:00Z == 1_709_251_200.
        let after = 1_709_251_200_i64;
        transport.expect(
            HttpMethod::Get,
            format!("https://api.test/mangopay/client-1/wallets/wallet-1/transactions?page=1&per_page=2&Sort=CreationDate:ASC&Type=PAYIN&AfterDate={after}"),
            page_json(
                &serde_json::json!([
                    {"Id": "o-10", "Type": "PAYIN", "CreationDate": 1_709_251_200_i64},
                    {"Id": "o-11", "Type": "PAYIN", "CreationDate": 1_717_200_000_i64}
                ]),
                1,
            ),
        );
        let c = MangoPayConnector::new(ConnectorInstanceId::new_v4(), transport, oauth())
            .with_page_size(2);
        let tok = c.authenticate(&cfg()).unwrap();
        let mut state = SyncState::new(c.instance);
        state.cursor = Some("2024-03-01T00:00:00+00:00".to_string());
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        assert_eq!(res.events.len(), 1);
        assert_eq!(
            res.next_cursor.as_deref(),
            Some("2024-06-01T00:00:00+00:00")
        );
    }

    #[test]
    fn fetch_content_renders_markdown() {
        let transport = Arc::new(MockHttpTransport::new());
        expect_token(&transport, "https://api.test/mangopay");
        transport.expect(
            HttpMethod::Get,
            "https://api.test/mangopay/client-1/payins/o-1",
            ok_json(&serde_json::json!({
                "Id": "o-1",
                "Status": "SUCCEEDED",
                "Type": "PAYIN",
                "Tag": "order-4242"
            })),
        );
        let c = MangoPayConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let content = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("o-1"))
            .unwrap();
        let body = String::from_utf8(content.body).unwrap();
        assert!(body.contains("# Mangopay pay-in o-1"));
        assert!(body.contains("order-4242"));
        assert!(body.contains("SUCCEEDED"));
    }

    #[test]
    fn subscribe_webhook_is_polling_only() {
        let transport = Arc::new(MockHttpTransport::new());
        expect_token(&transport, "https://api.test/mangopay");
        let c = MangoPayConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let sub = c
            .subscribe_webhook(&cfg(), &tok, "https://substrate.example/webhooks/mangopay")
            .unwrap();
        assert!(sub.provider_subscription_id.is_none());
        assert_eq!(sub.secret.expose(), "mangopay-secret");
    }

    #[test]
    fn handle_webhook_event_parses_single() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = MangoPayConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let body = serde_json::to_vec(&serde_json::json!({
            "RessourceId": "payin-42",
            "EventType": "PAYIN_NORMAL_SUCCEEDED"
        }))
        .unwrap();
        let events = c.handle_webhook_event(&body).unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ConnectorEvent::DocumentUpdated { document_id, .. } => {
                assert_eq!(document_id.as_str(), "payin-42");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn handle_webhook_event_maps_created() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = MangoPayConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let body = serde_json::to_vec(&serde_json::json!({
            "RessourceId": "payin-7",
            "EventType": "PAYIN_NORMAL_CREATED"
        }))
        .unwrap();
        let events = c.handle_webhook_event(&body).unwrap();
        match &events[0] {
            ConnectorEvent::DocumentCreated { document_id, .. } => {
                assert_eq!(document_id.as_str(), "payin-7");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    /// Regression guard: the production base URL already carries the
    /// `/v2.01` API version, and Mangopay resource paths are prefixed
    /// with the `ClientId`. The request path must hit
    /// `/v2.01/{ClientId}/wallets/{WalletId}/transactions` without a
    /// duplicated version segment. Exercises `DEFAULT_API_BASE_URL`
    /// (no override) because the other tests point at a versionless
    /// test host that would mask a version-duplication bug.
    #[test]
    fn production_base_url_does_not_duplicate_version() {
        let transport = Arc::new(MockHttpTransport::new());
        expect_token(&transport, "https://api.mangopay.com/v2.01");
        transport.expect(
            HttpMethod::Get,
            "https://api.mangopay.com/v2.01/client-1/wallets/wallet-1/transactions?page=1&per_page=2&Sort=CreationDate:ASC&Type=PAYIN",
            page_json(&serde_json::json!([]), 1),
        );
        let c = MangoPayConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth())
            .with_page_size(2);
        let prod_cfg =
            ConnectorConfig::new(ConnectorKind::MangoPay, AuthKind::ApiKey, ScopeId::new_v4())
                .with_auth_config(serde_json::json!({
                    "api_key": "mangopay-key",
                    "client_id": "client-1",
                    "wallet_id": "wallet-1"
                }));
        let tok = c.authenticate(&prod_cfg).unwrap();
        let _ = c.initial_sync(&prod_cfg, &tok);
        let recorded = transport.recorded();
        assert_eq!(
            recorded[1].url,
            "https://api.mangopay.com/v2.01/client-1/wallets/wallet-1/transactions?page=1&per_page=2&Sort=CreationDate:ASC&Type=PAYIN",
            "request URL must not duplicate the API version"
        );
    }
}
