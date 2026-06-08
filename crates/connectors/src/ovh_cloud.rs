//! OVHcloud connector — OVHcloud API (`https://eu.api.ovh.com/1.0`).
//!
//! OVHcloud — French cloud API (account services / billing).
//!
//! ## Authentication (OVHcloud request signature)
//!
//! OVHcloud does **not** accept a plain API key in a custom header.
//! Each request is signed with the application's credentials. The
//! connector reads `application_key`, `application_secret` and
//! `consumer_key` from `auth_config_json` and sends:
//!
//! * `X-Ovh-Application: <application_key>`
//! * `X-Ovh-Consumer: <consumer_key>`
//! * `X-Ovh-Timestamp: <unix-seconds>`
//! * `X-Ovh-Signature: "$1$" + SHA1_HEX(AS "+" CK "+" METHOD "+" URL "+" BODY "+" TS)`
//!
//! (`AS` = application secret, `CK` = consumer key). See
//! <https://help.ovhcloud.com/csm/en-api-getting-started-ovhcloud-api>.
//! When an `authorization_code` grant is configured instead, the
//! injected [`OAuth2CodeExchange`] is used and requests fall back to a
//! plain `Authorization: Bearer` header (OVHcloud's OAuth2 flow).
//!
//! ## Resource model (`/services`)
//!
//! `GET /services` returns a **bare JSON array of numeric service IDs**
//! (`long[]`) — there is no `{data:[…]}` envelope, no `limit`/`offset`
//! pagination and no per-row timestamps. So:
//!
//! * `initial_sync` lists every service ID and emits a creation event
//!   for each, recording the maximum ID as the cursor.
//! * `incremental_sync` re-lists the IDs and emits creation events for
//!   IDs greater than the cursor (OVHcloud assigns monotonically
//!   increasing service IDs, and the list carries no change timestamps,
//!   so a max-ID high-water mark is the only sound incremental signal).
//! * `fetch_content` GETs a single service
//!   (`GET /services/{serviceId}` → `services.expanded.Service`).
//! * OVHcloud has no push webhooks for `/services`, so
//!   `subscribe_webhook` records a polling-only subscription.

use std::sync::Arc;

use chrono::{Duration, Utc};
use connector_framework::{
    classify_failure, percent_encode_path_component, Connector, ConnectorConfig, ConnectorError,
    ConnectorEvent, ConnectorInstanceId, FetchedContent, HttpRequest, HttpTransport,
    OAuth2CodeExchange, OAuth2Token, Result, SourceDocumentId, SyncRunResult, SyncState,
    WebhookEventTypes, WebhookSecret, WebhookSubscription,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// Default OVHcloud API base URL (EU region; already carries `/1.0`).
pub const DEFAULT_API_BASE_URL: &str = "https://eu.api.ovh.com/1.0";
/// Default scope recorded on the synthesised credential token.
pub const DEFAULT_SCOPE: &str = "services";
/// `OAuth2Token::token_type` marker for the OVHcloud request-signature
/// credential, distinguishing it from an OAuth-issued bearer token.
pub const SIGNATURE_TOKEN_TYPE: &str = "OvhSignature";

/// Single expanded service (`GET /services/{serviceId}`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct OvhServiceDetail {
    #[serde(rename = "serviceId", default)]
    service_id: Option<i64>,
    #[serde(default)]
    resource: Option<OvhResource>,
    #[serde(default)]
    billing: Option<OvhBilling>,
    #[serde(default)]
    tags: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct OvhResource {
    #[serde(default)]
    name: Option<String>,
    #[serde(rename = "displayName", default)]
    display_name: Option<String>,
    #[serde(default)]
    state: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct OvhBilling {
    #[serde(rename = "expirationDate", default)]
    expiration_date: Option<String>,
    #[serde(rename = "nextBillingDate", default)]
    next_billing_date: Option<String>,
}

/// OVHcloud connector.
pub struct OvhCloudConnector {
    /// Connector instance id.
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    oauth: Arc<dyn OAuth2CodeExchange>,
    api_base_url: String,
    /// Fixed signing timestamp (tests only); production uses wall-clock.
    signing_timestamp: Option<i64>,
}

impl std::fmt::Debug for OvhCloudConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OvhCloudConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("signing_timestamp", &self.signing_timestamp)
            .field("transport", &"<HttpTransport>")
            .field("oauth", &"<OAuth2CodeExchange>")
            .finish()
    }
}

impl OvhCloudConnector {
    /// Construct a OVHcloud connector.
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
            signing_timestamp: None,
        }
    }

    /// Override the OVHcloud API base URL.
    #[must_use]
    pub fn with_api_base_url(mut self, url: impl Into<String>) -> Self {
        self.api_base_url = url.into();
        self
    }

    /// Pin the signing timestamp (test determinism).
    #[must_use]
    pub fn with_signing_timestamp(mut self, ts: i64) -> Self {
        self.signing_timestamp = Some(ts);
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

    /// Attach OVHcloud auth to a GET request: the request signature for
    /// the application-credential path, or `Authorization: Bearer` for
    /// the OAuth path.
    fn apply_auth(
        &self,
        req: HttpRequest,
        method: &str,
        url: &str,
        body: &[u8],
        config: &ConnectorConfig,
        token: &OAuth2Token,
    ) -> Result<HttpRequest> {
        if token.token_type != SIGNATURE_TOKEN_TYPE {
            let scheme = if token.token_type.is_empty() {
                "Bearer"
            } else {
                token.token_type.as_str()
            };
            return Ok(req.with_header(
                "Authorization",
                format!("{scheme} {}", token.access_token.expose()),
            ));
        }
        let app_key = config_str(config, "application_key").ok_or_else(|| {
            ConnectorError::Auth("ovh_cloud: auth_config_json.application_key is required".into())
        })?;
        let app_secret = config_str(config, "application_secret").ok_or_else(|| {
            ConnectorError::Auth(
                "ovh_cloud: auth_config_json.application_secret is required".into(),
            )
        })?;
        let consumer_key = config_str(config, "consumer_key").ok_or_else(|| {
            ConnectorError::Auth("ovh_cloud: auth_config_json.consumer_key is required".into())
        })?;
        let ts = self
            .signing_timestamp
            .unwrap_or_else(|| Utc::now().timestamp());
        let signature = ovh_signature(app_secret, consumer_key, method, url, body, ts);
        Ok(req
            .with_header("X-Ovh-Application", app_key)
            .with_header("X-Ovh-Consumer", consumer_key)
            .with_header("X-Ovh-Timestamp", ts.to_string())
            .with_header("X-Ovh-Signature", signature))
    }

    fn http_get<R: DeserializeOwned>(
        &self,
        endpoint: &str,
        url: &str,
        config: &ConnectorConfig,
        token: &OAuth2Token,
    ) -> Result<R> {
        let req = self.apply_auth(
            HttpRequest::get(url).with_header("Accept", "application/json"),
            "GET",
            url,
            b"",
            config,
            token,
        )?;
        let resp = self.transport.execute(req)?;
        if !resp.is_success() {
            return Err(classify_failure("ovh_cloud", endpoint, &resp));
        }
        serde_json::from_slice::<R>(&resp.body).map_err(|e| {
            ConnectorError::Sync(format!(
                "ovh_cloud {endpoint} JSON parse failed: {e} (body prefix: {})",
                String::from_utf8_lossy(&resp.body[..resp.body.len().min(256)])
            ))
        })
    }

    /// `GET /services` → bare array of numeric service IDs.
    fn list_service_ids(
        &self,
        base_url: &str,
        config: &ConnectorConfig,
        token: &OAuth2Token,
    ) -> Result<Vec<i64>> {
        let url = format!("{base_url}/services");
        self.http_get("/services", &url, config, token)
    }
}

fn config_str<'a>(config: &'a ConnectorConfig, key: &str) -> Option<&'a str> {
    config
        .auth_config_json
        .get(key)
        .and_then(serde_json::Value::as_str)
}

/// Build the OVHcloud `X-Ovh-Signature` value:
/// `"$1$" + SHA1_HEX(AS "+" CK "+" METHOD "+" URL "+" BODY "+" TS)`.
fn ovh_signature(
    app_secret: &str,
    consumer_key: &str,
    method: &str,
    url: &str,
    body: &[u8],
    timestamp: i64,
) -> String {
    let mut to_sign = String::new();
    to_sign.push_str(app_secret);
    to_sign.push('+');
    to_sign.push_str(consumer_key);
    to_sign.push('+');
    to_sign.push_str(method);
    to_sign.push('+');
    to_sign.push_str(url);
    to_sign.push('+');
    to_sign.push_str(&String::from_utf8_lossy(body));
    to_sign.push('+');
    to_sign.push_str(&timestamp.to_string());
    format!("$1${}", sha1_hex(to_sign.as_bytes()))
}

/// Lowercase-hex SHA-1 of `data`. Inlined per the codebase convention
/// of not adding a dependency for a small primitive (see
/// `content::decode_base64` / `signing::hex_lower`); OVHcloud's request
/// signature is the only consumer.
#[allow(clippy::many_single_char_names)] // standard SHA-1 working-variable names (FIPS 180-1)
fn sha1_hex(data: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut h: [u32; 5] = [
        0x6745_2301,
        0xEFCD_AB89,
        0x98BA_DCFE,
        0x1032_5476,
        0xC3D2_E1F0,
    ];
    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    let mut w = [0u32; 80];
    for chunk in msg.chunks_exact(64) {
        for (i, word) in w.iter_mut().take(16).enumerate() {
            *word = u32::from_be_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (i, &word) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A82_7999u32),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let tmp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = tmp;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }

    let mut out = String::with_capacity(40);
    for word in h {
        for byte in word.to_be_bytes() {
            out.push(HEX[(byte >> 4) as usize] as char);
            out.push(HEX[(byte & 0x0f) as usize] as char);
        }
    }
    out
}

impl Connector for OvhCloudConnector {
    fn authenticate(&self, config: &ConnectorConfig) -> Result<OAuth2Token> {
        // Application-credential (request-signature) path.
        if let Some(consumer_key) = config_str(config, "consumer_key") {
            if config_str(config, "application_key").is_none()
                || config_str(config, "application_secret").is_none()
            {
                return Err(ConnectorError::Auth(
                    "ovh_cloud authenticate: consumer_key requires application_key and application_secret"
                        .into(),
                ));
            }
            // The token carries the consumer key + a provenance marker;
            // the full credential triple is read from config at signing
            // time (per-request signature, not a static bearer).
            let mut token = OAuth2Token::new_without_refresh(
                consumer_key,
                Utc::now() + Duration::days(365),
                DEFAULT_SCOPE,
            );
            token.token_type = SIGNATURE_TOKEN_TYPE.to_string();
            return Ok(token);
        }
        let auth_code = config_str(config, "authorization_code").ok_or_else(|| {
            ConnectorError::Auth(
                "ovh_cloud authenticate: auth_config_json.consumer_key (+ application_key/secret) or .authorization_code is required"
                    .into(),
            )
        })?;
        self.oauth.exchange_code(config, auth_code)
    }

    fn initial_sync(&self, config: &ConnectorConfig, token: &OAuth2Token) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        let ids = self.list_service_ids(&base_url, config, token)?;
        let now = Utc::now();
        let mut events = Vec::with_capacity(ids.len());
        let mut max_id: Option<i64> = None;
        for id in ids {
            events.push(ConnectorEvent::DocumentCreated {
                document_id: SourceDocumentId::new(id.to_string()),
                occurred_at: now,
            });
            max_id = Some(max_id.map_or(id, |m| m.max(id)));
        }
        Ok(SyncRunResult {
            events,
            next_cursor: max_id.map(|m| m.to_string()),
        })
    }

    fn incremental_sync(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        state: &SyncState,
    ) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        let prior: Option<i64> = state.cursor.as_deref().and_then(|s| s.parse::<i64>().ok());
        let ids = self.list_service_ids(&base_url, config, token)?;
        let now = Utc::now();
        let mut events = Vec::new();
        let mut max_id = prior;
        for id in ids {
            // OVHcloud service IDs increase monotonically and the list
            // carries no timestamps, so anything past the high-water
            // mark is a newly provisioned service.
            if prior.is_none_or(|p| id > p) {
                events.push(ConnectorEvent::DocumentCreated {
                    document_id: SourceDocumentId::new(id.to_string()),
                    occurred_at: now,
                });
            }
            max_id = Some(max_id.map_or(id, |m| m.max(id)));
        }
        Ok(SyncRunResult {
            events,
            next_cursor: max_id.map(|m| m.to_string()),
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
        let url = format!("{base_url}/services/{id_enc}");
        let record: OvhServiceDetail = self.http_get("/services/{id}", &url, config, token)?;
        let resource = record.resource.clone().unwrap_or_default();
        let billing = record.billing.clone().unwrap_or_default();
        let display = resource
            .display_name
            .clone()
            .or_else(|| resource.name.clone())
            .unwrap_or_else(|| format!("service {id}"));
        let state = resource.state.as_deref().unwrap_or("unknown");
        let expiration = billing.expiration_date.as_deref().unwrap_or("(none)");
        let body = format!(
            "# OVHcloud service {id}\n\nName: {display}\nState: {state}\nExpiration: {expiration}\n"
        );
        Ok(FetchedContent::text(body, "text/markdown")
            .with_title(format!("OVHcloud service {display}"))
            .with_metadata(serde_json::json!({
                "provider": "ovh_cloud",
                "service_id": record.service_id,
                "state": resource.state,
                "expiration_date": billing.expiration_date,
                "next_billing_date": billing.next_billing_date,
                "tags": record.tags,
            })))
    }

    fn subscribe_webhook(
        &self,
        config: &ConnectorConfig,
        _token: &OAuth2Token,
        callback_url: &str,
    ) -> Result<WebhookSubscription> {
        // OVHcloud has no push-webhook REST endpoint for `/services`;
        // record a polling-only subscription so the runtime falls back
        // to incremental_sync.
        let secret = config_str(config, "webhook_secret").unwrap_or("ovh_cloud-webhook-secret");
        Ok(WebhookSubscription::new(
            self.instance,
            callback_url,
            WebhookSecret::new(secret),
            WebhookEventTypes::all(),
            None,
        ))
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        // OVHcloud does not deliver push notifications for `/services`;
        // this is a defensive parser in case a custom relay forwards a
        // `{serviceId, event}` payload. Sync is otherwise poll-based.
        #[derive(Deserialize)]
        struct OvhWebhookEvent {
            #[serde(rename = "serviceId", alias = "service_id", default)]
            service_id: serde_json::Value,
            #[serde(default)]
            event: String,
        }
        let deliveries: Vec<OvhWebhookEvent> =
            if let Ok(batch) = serde_json::from_slice::<Vec<OvhWebhookEvent>>(body) {
                batch
            } else {
                vec![
                    serde_json::from_slice::<OvhWebhookEvent>(body).map_err(|e| {
                        ConnectorError::Webhook(format!("ovh_cloud webhook parse failed: {e}"))
                    })?,
                ]
            };
        if deliveries.is_empty() {
            return Err(ConnectorError::Webhook(
                "empty ovh_cloud webhook batch".into(),
            ));
        }
        let mut events = Vec::with_capacity(deliveries.len());
        for delivery in deliveries {
            let id_str = id_value_to_string(&delivery.service_id).ok_or_else(|| {
                ConnectorError::Webhook("ovh_cloud webhook event missing serviceId".into())
            })?;
            let id = SourceDocumentId::new(id_str);
            let occurred_at = Utc::now();
            let event = if delivery.event.contains("create") {
                ConnectorEvent::DocumentCreated {
                    document_id: id,
                    occurred_at,
                }
            } else if delivery.event.contains("delete") || delivery.event.contains("terminat") {
                ConnectorEvent::DocumentDeleted {
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

fn id_value_to_string(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) if !s.is_empty() => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
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
                "services",
            ))
        }
    }

    fn oauth() -> Arc<dyn OAuth2CodeExchange> {
        Arc::new(FixedOAuth)
    }

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(ConnectorKind::OvhCloud, AuthKind::ApiKey, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({
                "application_key": "app-key",
                "application_secret": "app-secret",
                "consumer_key": "consumer-key",
                "api_base_url": "https://api.test/ovh",
                "webhook_secret": "ovh_cloud-secret",
            }))
    }

    fn cfg_oauth() -> ConnectorConfig {
        ConnectorConfig::new(ConnectorKind::OvhCloud, AuthKind::OAuth2, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({
                "authorization_code": "auth-code",
                "api_base_url": "https://api.test/ovh",
                "webhook_secret": "ovh_cloud-secret",
            }))
    }

    fn ok_json(value: &serde_json::Value) -> MockResponse {
        MockResponse::ok_json(serde_json::to_vec(value).unwrap())
    }

    #[test]
    fn sha1_known_vectors() {
        assert_eq!(sha1_hex(b""), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(sha1_hex(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(
            sha1_hex(b"The quick brown fox jumps over the lazy dog"),
            "2fd4e1c67a2d28fced849ee1bb76e7391b93eb12"
        );
    }

    #[test]
    fn authenticate_reads_signature_credentials() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = OvhCloudConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let token = c.authenticate(&cfg()).unwrap();
        assert_eq!(token.access_token.expose(), "consumer-key");
        assert_eq!(token.token_type, SIGNATURE_TOKEN_TYPE);
    }

    #[test]
    fn authenticate_falls_back_to_oauth_code() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = OvhCloudConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let token = c.authenticate(&cfg_oauth()).unwrap();
        assert_eq!(token.access_token.expose(), "unused");
        assert_eq!(token.token_type, "Bearer");
    }

    #[test]
    fn authenticate_requires_credential() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = OvhCloudConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let bare =
            ConnectorConfig::new(ConnectorKind::OvhCloud, AuthKind::ApiKey, ScopeId::new_v4());
        assert!(matches!(
            c.authenticate(&bare),
            Err(ConnectorError::Auth(_))
        ));
    }

    #[test]
    fn authenticate_consumer_key_requires_app_credentials() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = OvhCloudConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let cfg =
            ConnectorConfig::new(ConnectorKind::OvhCloud, AuthKind::ApiKey, ScopeId::new_v4())
                .with_auth_config(serde_json::json!({ "consumer_key": "ck" }));
        assert!(matches!(c.authenticate(&cfg), Err(ConnectorError::Auth(_))));
    }

    #[test]
    fn initial_sync_lists_ids_and_signs_request() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/ovh/services",
            ok_json(&serde_json::json!([101, 202, 303])),
        );
        let c = OvhCloudConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth())
            .with_signing_timestamp(1_700_000_000);
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 3);
        assert_eq!(res.next_cursor.as_deref(), Some("303"));

        let recorded = transport.recorded();
        let headers = &recorded[0].headers;
        // The four OVHcloud signature headers must be present, and the
        // signature must match the documented formula exactly.
        let expected_sig = ovh_signature(
            "app-secret",
            "consumer-key",
            "GET",
            "https://api.test/ovh/services",
            b"",
            1_700_000_000,
        );
        assert!(headers
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("x-ovh-application") && v == "app-key"));
        assert!(headers
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("x-ovh-consumer") && v == "consumer-key"));
        assert!(headers
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("x-ovh-timestamp") && v == "1700000000"));
        assert!(headers
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("x-ovh-signature") && *v == expected_sig));
        assert!(expected_sig.starts_with("$1$"));
        // The bogus template header must be gone.
        assert!(!headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("X-OvhCloud-Api-Key")));
    }

    #[test]
    fn incremental_sync_emits_only_new_ids() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/ovh/services",
            ok_json(&serde_json::json!([101, 202, 303, 404])),
        );
        let c = OvhCloudConnector::new(ConnectorInstanceId::new_v4(), transport, oauth())
            .with_signing_timestamp(1_700_000_000);
        let tok = c.authenticate(&cfg()).unwrap();
        let mut state = SyncState::new(c.instance);
        state.cursor = Some("303".to_string());
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        assert_eq!(res.events.len(), 1);
        assert_eq!(res.next_cursor.as_deref(), Some("404"));
        match &res.events[0] {
            ConnectorEvent::DocumentCreated { document_id, .. } => {
                assert_eq!(document_id.as_str(), "404");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn fetch_content_renders_markdown() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/ovh/services/101",
            ok_json(&serde_json::json!({
                "serviceId": 101,
                "resource": { "displayName": "my-vps.example", "name": "vps-101", "state": "active" },
                "billing": { "expirationDate": "2025-01-01T00:00:00+00:00" },
                "tags": ["prod"]
            })),
        );
        let c = OvhCloudConnector::new(ConnectorInstanceId::new_v4(), transport, oauth())
            .with_signing_timestamp(1_700_000_000);
        let tok = c.authenticate(&cfg()).unwrap();
        let content = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("101"))
            .unwrap();
        let body = String::from_utf8(content.body).unwrap();
        assert!(body.contains("# OVHcloud service 101"));
        assert!(body.contains("my-vps.example"));
        assert!(body.contains("active"));
    }

    #[test]
    fn subscribe_webhook_is_polling_only() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = OvhCloudConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let sub = c
            .subscribe_webhook(&cfg(), &tok, "https://substrate.example/webhooks/ovh_cloud")
            .unwrap();
        assert!(sub.provider_subscription_id.is_none());
        assert_eq!(sub.secret.expose(), "ovh_cloud-secret");
    }

    #[test]
    fn handle_webhook_event_parses_single() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = OvhCloudConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let body =
            serde_json::to_vec(&serde_json::json!({ "serviceId": 42, "event": "service.updated" }))
                .unwrap();
        let events = c.handle_webhook_event(&body).unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ConnectorEvent::DocumentUpdated { document_id, .. } => {
                assert_eq!(document_id.as_str(), "42");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    /// Regression guard: the production base URL already carries the
    /// `/1.0` API version, so the request path must NOT re-introduce a
    /// version segment. Exercises `DEFAULT_API_BASE_URL` (no override)
    /// because the other tests point at a versionless test host.
    #[test]
    fn production_base_url_does_not_duplicate_version() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://eu.api.ovh.com/1.0/services",
            ok_json(&serde_json::json!([])),
        );
        let c = OvhCloudConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth())
            .with_signing_timestamp(1_700_000_000);
        let prod_cfg =
            ConnectorConfig::new(ConnectorKind::OvhCloud, AuthKind::ApiKey, ScopeId::new_v4())
                .with_auth_config(serde_json::json!({
                    "application_key": "app-key",
                    "application_secret": "app-secret",
                    "consumer_key": "consumer-key"
                }));
        let tok = c.authenticate(&prod_cfg).unwrap();
        let _ = c.initial_sync(&prod_cfg, &tok);
        let recorded = transport.recorded();
        assert_eq!(
            recorded[0].url, "https://eu.api.ovh.com/1.0/services",
            "request URL must not duplicate the API version"
        );
    }
}
