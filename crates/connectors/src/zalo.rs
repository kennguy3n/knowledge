//! Zalo connector — Zalo Official Account Open API + webhooks.
//!
//! Zalo is Vietnam's dominant messaging platform; an Official Account
//! (OA) exposes broadcast articles, follower lists, and the
//! conversation feed through the Open API at `https://openapi.zalo.me`.
//!
//! * `initial_sync` walks `GET /v2.0/article/getslice` and pages via
//!   the `offset` / `limit` cursor (descending publish time).
//! * `incremental_sync` re-walks the same feed and filters by the
//!   stored `time` watermark (the slice endpoint has no server-side
//!   `since` filter, so the boundary is enforced client-side).
//! * `fetch_content` reads `GET /v2.0/article/getdetail?id={id}` and
//!   renders a Markdown summary.
//! * `subscribe_webhook` surfaces the operator-provided OA secret —
//!   Zalo webhooks are configured once in the Zalo Developer console
//!   (there is no create-webhook REST endpoint), so no HTTP call is
//!   issued; the secret lets the substrate validate the `X-ZEvent-Signature`
//!   `mac` on delivery.
//! * `handle_webhook_event` parses an OA event payload; Zalo carries
//!   the event name in the `event_name` body field
//!   (`user_send_text`, `article_create`, …).
//!
//! Zalo authenticates Open API calls with an `access_token` header
//! (not a bearer `Authorization`), so the connector issues requests
//! through the injected [`HttpTransport`] directly rather than the
//! bearer helpers. `authenticate` accepts a configured `access_token`
//! (the common case for an installed OA) or an OAuth2
//! `authorization_code` exchanged through the injected
//! [`OAuth2CodeExchange`].

use std::fmt::Write as _;
use std::sync::Arc;

use chrono::{DateTime, TimeZone, Utc};
use connector_framework::{
    classify_failure, percent_encode_path_component, Connector, ConnectorConfig, ConnectorError,
    ConnectorEvent, ConnectorInstanceId, FetchedContent, HttpRequest, HttpTransport,
    OAuth2CodeExchange, OAuth2Token, Result, SourceDocumentId, SyncRunResult, SyncState,
    WatermarkCursor, WebhookEventTypes, WebhookSecret, WebhookSubscription,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// Default Zalo Open API base URL.
pub const DEFAULT_API_BASE_URL: &str = "https://openapi.zalo.me";

/// Page size for the article slice endpoint. Zalo's documented max
/// is 50.
pub const DEFAULT_PAGE_SIZE: u32 = 50;

/// Safety ceiling on list pages walked in one sync run.
pub const MAX_LIST_PAGES: usize = 10_000;

/// Scope recorded on a token synthesised from a configured access token.
const DEFAULT_SCOPE: &str = "oa.manage";

/// One Zalo OA article (subset of fields the substrate ingests).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ZaloArticle {
    /// Article id (token string).
    pub id: String,
    /// Article title.
    #[serde(default)]
    pub title: Option<String>,
    /// Article type (`normal`, `video`, …).
    #[serde(default, rename = "type")]
    pub article_type: Option<String>,
    /// Publish/update time in epoch milliseconds.
    #[serde(default)]
    pub time: Option<i64>,
    /// Public article URL, when present.
    #[serde(default)]
    pub url: Option<String>,
}

/// `data` envelope for the article slice endpoint.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ZaloArticleData {
    /// Articles on this page. Zalo names the array `medias`.
    #[serde(default, alias = "medias")]
    pub articles: Vec<ZaloArticle>,
}

/// Envelope for the article slice endpoint.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ZaloArticlesResponse {
    /// Provider error code (`0` on success).
    #[serde(default)]
    pub error: i64,
    /// Provider error message.
    #[serde(default)]
    pub message: Option<String>,
    /// Slice payload.
    #[serde(default)]
    pub data: ZaloArticleData,
}

/// `data` envelope for the article detail endpoint.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ZaloArticleDetailResponse {
    /// Provider error code (`0` on success).
    #[serde(default)]
    pub error: i64,
    /// The article.
    #[serde(default)]
    pub data: ZaloArticle,
}

/// Webhook event payload. Zalo posts a flat object carrying the
/// event name and the affected entity id.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ZaloWebhookPayload {
    /// Event name (`article_create`, `article_update`,
    /// `article_delete`, `user_send_text`, …).
    #[serde(default)]
    pub event_name: Option<String>,
    /// App/OA id.
    #[serde(default)]
    pub app_id: Option<String>,
    /// Event timestamp in epoch milliseconds (string per Zalo's wire).
    #[serde(default)]
    pub timestamp: Option<String>,
    /// Affected article, when the event carries one.
    #[serde(default)]
    pub article: Option<ZaloWebhookArticle>,
}

/// Article reference inside a webhook payload.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ZaloWebhookArticle {
    /// Article id.
    #[serde(default)]
    pub id: String,
}

/// Zalo Official Account connector.
pub struct ZaloConnector {
    /// Connector instance id.
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    oauth: Arc<dyn OAuth2CodeExchange>,
    api_base_url: String,
    page_size: u32,
}

impl std::fmt::Debug for ZaloConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ZaloConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("page_size", &self.page_size)
            .field("transport", &"<HttpTransport>")
            .field("oauth", &"<OAuth2CodeExchange>")
            .finish()
    }
}

impl ZaloConnector {
    /// Construct a Zalo connector.
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

    /// Override the Open API base URL.
    #[must_use]
    pub fn with_api_base_url(mut self, url: impl Into<String>) -> Self {
        self.api_base_url = url.into();
        self
    }

    /// Override the list page size. Clamped to `[1, 50]`.
    #[must_use]
    pub fn with_page_size(mut self, size: u32) -> Self {
        self.page_size = size.clamp(1, 50);
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

    /// GET a JSON endpoint with Zalo's `access_token` header and parse
    /// the response, mapping a non-zero provider `error` code onto a
    /// sync error.
    fn zalo_get<R: DeserializeOwned>(
        &self,
        endpoint: &str,
        url: &str,
        token: &OAuth2Token,
    ) -> Result<R> {
        let req = HttpRequest::get(url)
            .with_header("Accept", "application/json")
            .with_header("access_token", token.access_token.expose());
        let resp = self.transport.execute(req)?;
        if !resp.is_success() {
            return Err(classify_failure("zalo", endpoint, &resp));
        }
        serde_json::from_slice::<R>(&resp.body).map_err(|e| {
            ConnectorError::Sync(format!(
                "zalo {endpoint} JSON parse failed: {e} (body prefix: {})",
                String::from_utf8_lossy(&resp.body[..resp.body.len().min(256)])
            ))
        })
    }

    /// Walk every article page until a short page is returned, paging
    /// by ascending `offset`.
    fn paginate_articles(&self, base_url: &str, token: &OAuth2Token) -> Result<Vec<ZaloArticle>> {
        let mut out = Vec::<ZaloArticle>::new();
        let mut offset: usize = 0;
        for _ in 0..MAX_LIST_PAGES {
            let url = format!(
                "{base_url}/v2.0/article/getslice?offset={offset}&limit={}",
                self.page_size
            );
            let resp: ZaloArticlesResponse =
                self.zalo_get("/v2.0/article/getslice", &url, token)?;
            if resp.error != 0 {
                return Err(ConnectorError::Sync(format!(
                    "zalo /v2.0/article/getslice returned error {}: {}",
                    resp.error,
                    resp.message.unwrap_or_default()
                )));
            }
            let returned = resp.data.articles.len();
            out.extend(resp.data.articles);
            if returned < self.page_size as usize {
                return Ok(out);
            }
            offset += returned;
        }
        Err(ConnectorError::Sync(format!(
            "zalo /v2.0/article/getslice exceeded {MAX_LIST_PAGES} pages"
        )))
    }
}

/// Convert an epoch-millisecond timestamp into a UTC datetime.
fn epoch_millis_to_utc(ms: i64) -> Option<DateTime<Utc>> {
    Utc.timestamp_millis_opt(ms).single()
}

fn article_to_event(a: &ZaloArticle, kind: &str) -> ConnectorEvent {
    let occurred_at = a
        .time
        .and_then(epoch_millis_to_utc)
        .unwrap_or_else(Utc::now);
    let id = SourceDocumentId::new(a.id.clone());
    match kind {
        "delete" => ConnectorEvent::DocumentDeleted {
            document_id: id,
            occurred_at,
        },
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

impl Connector for ZaloConnector {
    fn authenticate(&self, config: &ConnectorConfig) -> Result<OAuth2Token> {
        if let Some(key) = config
            .auth_config_json
            .get("access_token")
            .and_then(serde_json::Value::as_str)
        {
            return Ok(OAuth2Token::new_without_refresh(
                key,
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
                    "zalo authenticate: auth_config_json.access_token or .authorization_code is required"
                        .into(),
                )
            })?;
        self.oauth.exchange_code(config, auth_code)
    }

    fn initial_sync(&self, config: &ConnectorConfig, token: &OAuth2Token) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        let articles = self.paginate_articles(&base_url, token)?;
        let mut events = Vec::with_capacity(articles.len());
        let mut cursor = WatermarkCursor::empty();
        for a in &articles {
            events.push(article_to_event(a, "create"));
            if let Some(t) = a.time.and_then(epoch_millis_to_utc) {
                cursor.observe(t, &a.id);
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
        let prior = WatermarkCursor::parse(state.cursor.as_deref());
        let articles = self.paginate_articles(&base_url, token)?;
        let mut events = Vec::new();
        let mut cursor = prior.clone();
        for a in &articles {
            // The slice endpoint has no server-side `since` filter. The
            // epoch-millis watermark is carried through a `WatermarkCursor`
            // (article time ↔ `DateTime<Utc>`) so boundary articles sharing
            // the watermark instant are deduped by id while brand-new ones
            // are still surfaced.
            let Some(t) = a.time.and_then(epoch_millis_to_utc) else {
                continue;
            };
            if !prior.should_emit(t, &a.id) {
                continue;
            }
            events.push(article_to_event(a, "update"));
            cursor.observe(t, &a.id);
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
        let id = document_id.as_str();
        let id_enc = percent_encode_path_component(id);
        let url = format!("{base_url}/v2.0/article/getdetail?id={id_enc}");
        let resp: ZaloArticleDetailResponse =
            self.zalo_get("/v2.0/article/getdetail", &url, token)?;
        if resp.error != 0 {
            return Err(ConnectorError::Sync(format!(
                "zalo /v2.0/article/getdetail returned error {}",
                resp.error
            )));
        }
        let article = resp.data;

        let title = article
            .title
            .clone()
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| format!("Article {id}"));
        let mut md = String::new();
        md.push_str("# ");
        md.push_str(&title);
        md.push_str("\n\n");
        if let Some(kind) = article.article_type.as_deref().filter(|s| !s.is_empty()) {
            md.push_str("**Type:** ");
            md.push_str(kind);
            md.push_str("\n\n");
        }
        if let Some(url) = article.url.as_deref().filter(|s| !s.is_empty()) {
            let _ = write!(md, "**URL:** {url}\n\n");
        }
        let body = md.trim_end().to_string();

        let mut content = FetchedContent::text(body, "text/markdown")
            .with_title(title)
            .with_metadata(serde_json::json!({
                "provider": "zalo",
                "article_id": id,
                "type": article.article_type,
            }));
        if let Some(url) = article.url.filter(|s| !s.is_empty()) {
            content = content.with_source_url(url);
        }
        Ok(content)
    }

    fn subscribe_webhook(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        callback_url: &str,
    ) -> Result<WebhookSubscription> {
        // Zalo OA webhooks are configured once in the Zalo Developer
        // console (there is no create-webhook REST endpoint), so we do
        // not issue an HTTP call here. Surface the OA secret so the
        // substrate can validate the `X-ZEvent-Signature` `mac` on
        // delivery.
        let _ = token;
        let secret = config
            .auth_config_json
            .get("oa_secret")
            .or_else(|| config.auth_config_json.get("webhook_secret"))
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ConnectorError::Webhook(
                    "zalo subscribe_webhook: auth_config_json.oa_secret is required".into(),
                )
            })?;
        Ok(WebhookSubscription::new(
            self.instance,
            callback_url,
            WebhookSecret::new(secret),
            WebhookEventTypes::all(),
            None,
        ))
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        let payload: ZaloWebhookPayload = serde_json::from_slice(body)?;
        let article_id = payload
            .article
            .as_ref()
            .map(|a| a.id.clone())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                ConnectorError::Webhook("zalo webhook payload missing article id".into())
            })?;
        let occurred_at = payload
            .timestamp
            .as_deref()
            .and_then(|s| s.parse::<i64>().ok())
            .and_then(epoch_millis_to_utc)
            .unwrap_or_else(Utc::now);
        let id = SourceDocumentId::new(article_id);
        let event = match payload.event_name.as_deref() {
            Some("article_create") => ConnectorEvent::DocumentCreated {
                document_id: id,
                occurred_at,
            },
            Some("article_delete") => ConnectorEvent::DocumentDeleted {
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
                "zalo-access",
                "zalo-refresh",
                Utc::now() + Duration::hours(1),
                "oa.manage",
            ))
        }
    }

    fn oauth() -> Arc<dyn OAuth2CodeExchange> {
        Arc::new(FixedOAuth)
    }

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(ConnectorKind::Zalo, AuthKind::ApiKey, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({
                "access_token": "zalo_tok_123",
                "api_base_url": "https://api.test/zalo",
                "oa_secret": "zalo-oa-secret",
            }))
    }

    fn article(id: &str, time: i64) -> serde_json::Value {
        serde_json::json!({ "id": id, "title": format!("Post {id}"), "type": "normal", "time": time })
    }

    fn ok_json(value: &serde_json::Value) -> MockResponse {
        MockResponse::ok_json(serde_json::to_vec(value).unwrap())
    }

    #[test]
    fn authenticate_wraps_access_token() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = ZaloConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        assert_eq!(tok.access_token.expose(), "zalo_tok_123");
    }

    #[test]
    fn authenticate_falls_back_to_oauth_exchange() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = ZaloConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let cfg = ConnectorConfig::new(ConnectorKind::Zalo, AuthKind::OAuth2, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({ "authorization_code": "abc" }));
        let tok = c.authenticate(&cfg).unwrap();
        assert_eq!(tok.access_token.expose(), "zalo-access");
    }

    #[test]
    fn authenticate_requires_token_or_code() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = ZaloConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let bare = ConnectorConfig::new(ConnectorKind::Zalo, AuthKind::ApiKey, ScopeId::new_v4());
        assert!(matches!(
            c.authenticate(&bare).unwrap_err(),
            ConnectorError::Auth(_)
        ));
    }

    #[test]
    fn initial_sync_emits_created_with_access_token_header() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/zalo/v2.0/article/getslice?offset=0&limit=50",
            ok_json(&serde_json::json!({ "error": 0, "data": { "medias": [article("a1", 1_700_000_000_000)] } })),
        );
        let c = ZaloConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 1);
        assert!(matches!(
            res.events[0],
            ConnectorEvent::DocumentCreated { .. }
        ));
        // 1_700_000_000_000 ms == 2023-11-14T22:13:20 UTC; the cursor now
        // carries the boundary id alongside the RFC-3339 watermark.
        assert_eq!(
            res.next_cursor.as_deref(),
            Some("2023-11-14T22:13:20+00:00|a1")
        );
        let rec = transport.recorded();
        assert!(rec[0]
            .headers
            .iter()
            .any(|(k, v)| k == "access_token" && v == "zalo_tok_123"));
    }

    #[test]
    fn initial_sync_paginates_via_offset() {
        let transport = Arc::new(MockHttpTransport::new());
        let full: Vec<serde_json::Value> = (0..50)
            .map(|i| article(&format!("a{i}"), 1_700_000_000_000 + i64::from(i)))
            .collect();
        transport.expect(
            HttpMethod::Get,
            "https://api.test/zalo/v2.0/article/getslice?offset=0&limit=50",
            ok_json(&serde_json::json!({ "error": 0, "data": { "medias": full } })),
        );
        transport.expect(
            HttpMethod::Get,
            "https://api.test/zalo/v2.0/article/getslice?offset=50&limit=50",
            ok_json(&serde_json::json!({ "error": 0, "data": { "medias": [article("a50", 1_700_000_100_000)] } })),
        );
        let c = ZaloConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 51);
        assert_eq!(transport.recorded().len(), 2);
    }

    #[test]
    fn incremental_sync_filters_by_watermark() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/zalo/v2.0/article/getslice?offset=0&limit=50",
            ok_json(&serde_json::json!({ "error": 0, "data": { "medias": [
                article("old", 1_700_000_000_000),
                article("boundary_new", 1_700_000_000_000),
                article("new", 1_700_000_500_000),
            ] } })),
        );
        let c = ZaloConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let mut state = SyncState::new(c.instance);
        // Prior cursor: watermark at 1_700_000_000_000 ms with id "old" seen.
        state.cursor = Some("2023-11-14T22:13:20+00:00|old".to_string());
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        // "old" deduped; the brand-new same-instant "boundary_new" surfaces,
        // as does the strictly-newer "new".
        let ids: Vec<&str> = res
            .events
            .iter()
            .map(|e| e.document_id().as_str())
            .collect();
        assert_eq!(ids, vec!["boundary_new", "new"]);
    }

    #[test]
    fn initial_sync_maps_403_to_auth_error() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/zalo/v2.0/article/getslice?offset=0&limit=50",
            MockResponse::status(403, b"forbidden".to_vec()),
        );
        let c = ZaloConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        assert!(matches!(
            c.initial_sync(&cfg(), &tok).unwrap_err(),
            ConnectorError::Auth(_)
        ));
    }

    #[test]
    fn initial_sync_maps_provider_error_code_to_sync_error() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/zalo/v2.0/article/getslice?offset=0&limit=50",
            ok_json(&serde_json::json!({ "error": -201, "message": "access token is invalid" })),
        );
        let c = ZaloConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        assert!(matches!(
            c.initial_sync(&cfg(), &tok).unwrap_err(),
            ConnectorError::Sync(_)
        ));
    }

    #[test]
    fn fetch_content_renders_markdown() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/zalo/v2.0/article/getdetail?id=a1",
            ok_json(&serde_json::json!({ "error": 0, "data": {
                "id": "a1", "title": "Launch", "type": "normal",
                "url": "https://zalo.me/oa/a1",
            } })),
        );
        let c = ZaloConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let content = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("a1"))
            .unwrap();
        assert_eq!(content.title.as_deref(), Some("Launch"));
        assert_eq!(content.source_url.as_deref(), Some("https://zalo.me/oa/a1"));
        assert!(String::from_utf8(content.body)
            .unwrap()
            .contains("# Launch"));
    }

    #[test]
    fn subscribe_webhook_uses_oa_secret_without_http() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = ZaloConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let sub = c
            .subscribe_webhook(&cfg(), &tok, "https://hook.example/zalo")
            .unwrap();
        assert_eq!(sub.secret.expose(), "zalo-oa-secret");
        assert!(transport.recorded().is_empty());
    }

    #[test]
    fn webhook_create_event_maps_to_document_created() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = ZaloConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let body = serde_json::json!({
            "event_name": "article_create",
            "timestamp": "1700000000000",
            "article": { "id": "a9" },
        });
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert_eq!(evs.len(), 1);
        assert!(matches!(evs[0], ConnectorEvent::DocumentCreated { .. }));
        assert_eq!(evs[0].document_id().as_str(), "a9");
    }

    #[test]
    fn webhook_missing_article_id_errors() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = ZaloConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let body = serde_json::json!({ "event_name": "user_send_text" });
        assert!(matches!(
            c.handle_webhook_event(&serde_json::to_vec(&body).unwrap())
                .unwrap_err(),
            ConnectorError::Webhook(_)
        ));
    }
}
