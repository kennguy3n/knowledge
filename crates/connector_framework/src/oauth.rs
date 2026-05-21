//! Real OAuth2 client implementations.
//!
//! Provides reqwest-backed `OAuth2CodeExchange` and `TokenRefresher`
//! implementations that POST to a provider's `/token` endpoint with
//! `grant_type=authorization_code` and `grant_type=refresh_token`
//! respectively. Both are gated behind the `http-client` feature
//! flag so CI hosts without the reqwest build chain can still
//! compile the crate.
//!
//! The OAuth2 endpoint URL and `client_id` / `redirect_uri` are
//! read out of [`crate::config::ConnectorConfig::auth_config_json`]
//! (a flexible JSON blob — see `docs/DESIGN.md` §10.2). The
//! `client_secret` is **never** stored on disk; production callers
//! pass it in via [`ReqwestOAuth2Client::with_client_secret`] at
//! runtime from the OS keychain / secrets manager.

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::Deserialize;

use crate::config::ConnectorConfig;
use crate::error::{ConnectorError, Result};
use crate::http::{HttpRequest, HttpResponse, HttpTransport};
use crate::token_vault::{OAuth2CodeExchange, OAuth2Token, RefreshedToken, TokenRefresher};

/// Reqwest-backed OAuth2 client that drives both `authorization_code`
/// and `refresh_token` grants over a shared [`HttpTransport`].
///
/// One `ReqwestOAuth2Client` instance services every connector
/// using the same transport — the per-provider knobs (token URL,
/// `client_id`, `client_secret`) come in via the
/// [`ConnectorConfig`] / the explicit `with_*` setters. The client
/// is `Clone` so callers can share it across the connector
/// runtime's worker pool.
#[derive(Clone)]
pub struct ReqwestOAuth2Client<T: HttpTransport> {
    transport: std::sync::Arc<T>,
    client_secret: Option<String>,
}

impl<T: HttpTransport> std::fmt::Debug for ReqwestOAuth2Client<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReqwestOAuth2Client")
            .field("transport", &"<HttpTransport>")
            .field(
                "client_secret",
                &self.client_secret.as_ref().map(|_| "[redacted]"),
            )
            .finish()
    }
}

impl<T: HttpTransport> ReqwestOAuth2Client<T> {
    /// Wrap a transport. No client secret is set — callers that
    /// negotiate against providers requiring a confidential client
    /// (most major providers: Google, Microsoft, Atlassian, …)
    /// must call [`Self::with_client_secret`] before invoking
    /// `exchange_code` / `refresh`.
    pub fn new(transport: std::sync::Arc<T>) -> Self {
        Self {
            transport,
            client_secret: None,
        }
    }

    /// Provide the OAuth2 client secret (kept in memory only — the
    /// substrate never persists it). Chainable.
    #[must_use]
    pub fn with_client_secret(mut self, secret: impl Into<String>) -> Self {
        self.client_secret = Some(secret.into());
        self
    }

    fn token_url(config: &ConnectorConfig) -> Result<&str> {
        config
            .auth_config_json
            .get("token_url")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ConnectorError::Auth(
                    "auth_config_json.token_url is required for OAuth2 grants".into(),
                )
            })
    }

    fn client_id(config: &ConnectorConfig) -> Result<&str> {
        config
            .auth_config_json
            .get("client_id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ConnectorError::Auth("auth_config_json.client_id is required for OAuth2".into())
            })
    }

    fn redirect_uri(config: &ConnectorConfig) -> Result<&str> {
        config
            .auth_config_json
            .get("redirect_uri")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ConnectorError::Auth(
                    "auth_config_json.redirect_uri is required for authorization_code".into(),
                )
            })
    }

    fn execute_token_grant(&self, token_url: &str, form: &[(&str, &str)]) -> Result<TokenResponse> {
        // Manual urlencoded body — avoids pulling in a new
        // dependency just for OAuth2 form encoding.
        let body = encode_form(form);
        let req = HttpRequest::post(token_url, body.into_bytes())
            .with_header("Content-Type", "application/x-www-form-urlencoded")
            .with_header("Accept", "application/json");
        let resp = self.transport.execute(req)?;
        parse_token_response(&resp)
    }
}

impl<T: HttpTransport + 'static> OAuth2CodeExchange for ReqwestOAuth2Client<T> {
    fn exchange_code(&self, config: &ConnectorConfig, auth_code: &str) -> Result<OAuth2Token> {
        let token_url = Self::token_url(config)?;
        let client_id = Self::client_id(config)?;
        let redirect_uri = Self::redirect_uri(config)?;
        let secret = self.client_secret.as_deref().unwrap_or("");
        let form = [
            ("grant_type", "authorization_code"),
            ("code", auth_code),
            ("redirect_uri", redirect_uri),
            ("client_id", client_id),
            ("client_secret", secret),
        ];
        let resp = self.execute_token_grant(token_url, &form)?;
        Ok(token_response_to_oauth2(resp, Utc::now()))
    }
}

impl<T: HttpTransport + 'static> TokenRefresher for ReqwestOAuth2Client<T> {
    fn refresh(&self, _refresh_token: &str) -> Result<RefreshedToken> {
        Err(ConnectorError::TokenRefresh(
            "ReqwestOAuth2Client::refresh requires the per-connector ConnectorConfig — call \
             refresh_with_config(config, refresh_token) instead from the connector layer"
                .into(),
        ))
    }
}

impl<T: HttpTransport + 'static> ReqwestOAuth2Client<T> {
    /// Variant of [`TokenRefresher::refresh`] that takes the
    /// connector-specific `ConnectorConfig` (for `token_url` /
    /// `client_id`). The blanket `TokenRefresher` impl on this type
    /// requires the config, which the per-connector code already has
    /// — this is the entry point connectors actually call.
    ///
    /// # Errors
    ///
    /// Returns [`ConnectorError::Auth`] if the config is missing
    /// fields, [`ConnectorError::Transport`] on network failure,
    /// [`ConnectorError::TokenRefresh`] on provider rejection.
    pub fn refresh_with_config(
        &self,
        config: &ConnectorConfig,
        refresh_token: &str,
    ) -> Result<RefreshedToken> {
        let token_url = Self::token_url(config)?;
        let client_id = Self::client_id(config)?;
        let secret = self.client_secret.as_deref().unwrap_or("");
        let form = [
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", client_id),
            ("client_secret", secret),
        ];
        let resp = self.execute_token_grant(token_url, &form)?;
        Ok(token_response_to_refreshed(resp, Utc::now()))
    }
}

// ───────────── helpers ─────────────

/// Provider token-endpoint response. All major providers (Google,
/// Microsoft, Atlassian, Notion, Slack, HubSpot) return this shape;
/// fields the provider omits map to `None`.
#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    token_type: Option<String>,
}

fn parse_token_response(resp: &HttpResponse) -> Result<TokenResponse> {
    if !resp.is_success() {
        // Try to extract `error` / `error_description` from a JSON
        // OAuth2 error response; fall through to the raw body if
        // it's not JSON.
        let detail = serde_json::from_slice::<serde_json::Value>(&resp.body)
            .ok()
            .and_then(|v| {
                let err = v.get("error").and_then(serde_json::Value::as_str)?;
                let desc = v
                    .get("error_description")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                Some(format!("{err}: {desc}"))
            })
            .unwrap_or_else(|| String::from_utf8_lossy(&resp.body).to_string());
        return Err(ConnectorError::Auth(format!(
            "OAuth2 token endpoint returned status {} — {}",
            resp.status, detail
        )));
    }
    serde_json::from_slice::<TokenResponse>(&resp.body)
        .map_err(|e| ConnectorError::Auth(format!("token response not valid JSON: {e}")))
}

fn token_response_to_oauth2(resp: TokenResponse, now: DateTime<Utc>) -> OAuth2Token {
    let expires_at = now + expires_in_to_duration(resp.expires_in);
    let mut token = OAuth2Token::new(
        resp.access_token,
        resp.refresh_token.unwrap_or_default(),
        expires_at,
        resp.scope.unwrap_or_default(),
    );
    if let Some(t) = resp.token_type {
        token.token_type = t;
    }
    token
}

fn token_response_to_refreshed(resp: TokenResponse, now: DateTime<Utc>) -> RefreshedToken {
    RefreshedToken {
        access_token: resp.access_token,
        refresh_token: resp.refresh_token,
        expires_at: now + expires_in_to_duration(resp.expires_in),
        scope: resp.scope,
    }
}

fn expires_in_to_duration(expires_in: Option<i64>) -> ChronoDuration {
    // Providers that omit `expires_in` (e.g. Slack legacy tokens)
    // get a defensive 1-hour default — the vault will refresh as
    // needed.
    ChronoDuration::seconds(expires_in.unwrap_or(3600))
}

/// Minimal `application/x-www-form-urlencoded` encoder — handles
/// the subset of characters OAuth2 grant bodies need.
fn encode_form(pairs: &[(&str, &str)]) -> String {
    let mut out = String::new();
    for (i, (k, v)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push('&');
        }
        url_encode(k, &mut out);
        out.push('=');
        url_encode(v, &mut out);
    }
    out
}

fn url_encode(s: &str, out: &mut String) {
    for c in s.bytes() {
        match c {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(c as char)
            }
            b' ' => out.push('+'),
            _ => {
                use std::fmt::Write as _;
                let _ = write!(out, "%{c:02X}");
            }
        }
    }
}

/// Trivial timeout used by the reqwest client when nothing more
/// specific is set. Re-exported so callers don't have to know about
/// the `http` module to construct transports for OAuth2.
pub const DEFAULT_OAUTH_TIMEOUT_SECS: u64 = 30;

/// Build a default reqwest-backed OAuth2 client (transport + 30s
/// timeout + default retry policy). Convenience constructor for the
/// common production wiring path.
///
/// # Errors
///
/// Returns [`ConnectorError::Transport`] if the underlying reqwest
/// client builder rejects the timeout configuration.
#[cfg(feature = "http-client")]
pub fn default_oauth_client() -> Result<ReqwestOAuth2Client<crate::http::BlockingHttpTransport>> {
    let transport = std::sync::Arc::new(crate::http::BlockingHttpTransport::with_timeout(
        std::time::Duration::from_secs(DEFAULT_OAUTH_TIMEOUT_SECS),
    )?);
    Ok(ReqwestOAuth2Client::new(transport))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AuthKind, ConnectorKind};
    use crate::http::{HttpMethod, MockHttpTransport, MockResponse};
    use evidence_store::ScopeId;
    use std::sync::Arc;

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(ConnectorKind::Notion, AuthKind::OAuth2, ScopeId::new_v4())
            .with_auth_config(serde_json::json!({
                "token_url": "https://api.notion.com/v1/oauth/token",
                "client_id": "client-abc",
                "redirect_uri": "https://app.example.com/oauth/callback"
            }))
    }

    #[test]
    fn exchange_code_round_trip() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            "https://api.notion.com/v1/oauth/token",
            MockResponse::ok_json(
                br#"{"access_token":"AT","refresh_token":"RT","expires_in":3600,"scope":"read_content","token_type":"Bearer"}"#.to_vec(),
            ),
        );
        let client = ReqwestOAuth2Client::new(transport.clone()).with_client_secret("s3cret");
        let token = client.exchange_code(&cfg(), "code-xyz").expect("exchange");
        assert_eq!(token.access_token.expose(), "AT");
        assert_eq!(token.refresh_token.expose(), "RT");
        assert_eq!(token.scope, "read_content");
        assert_eq!(token.token_type, "Bearer");

        // Wire-level: form body is correctly encoded.
        let recorded = transport.recorded();
        assert_eq!(recorded.len(), 1);
        let body = String::from_utf8(recorded[0].body.clone()).expect("utf8");
        assert!(body.contains("grant_type=authorization_code"));
        assert!(body.contains("code=code-xyz"));
        assert!(body.contains("client_id=client-abc"));
        assert!(body.contains("client_secret=s3cret"));
        assert!(body.contains("redirect_uri=https%3A%2F%2Fapp.example.com%2Foauth%2Fcallback"));
        let ct = recorded[0]
            .headers
            .iter()
            .find(|(k, _)| k == "Content-Type")
            .map(|(_, v)| v.as_str());
        assert_eq!(ct, Some("application/x-www-form-urlencoded"));
    }

    #[test]
    fn refresh_with_config_round_trip() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            "https://api.notion.com/v1/oauth/token",
            MockResponse::ok_json(
                br#"{"access_token":"NEW","expires_in":7200,"scope":"read_content"}"#.to_vec(),
            ),
        );
        let client = ReqwestOAuth2Client::new(transport.clone()).with_client_secret("s3cret");
        let refreshed = client
            .refresh_with_config(&cfg(), "RT-OLD")
            .expect("refresh");
        assert_eq!(refreshed.access_token, "NEW");
        assert_eq!(refreshed.scope.as_deref(), Some("read_content"));
        // Provider didn't return a new refresh_token — keeper.
        assert_eq!(refreshed.refresh_token, None);

        let recorded = transport.recorded();
        let body = String::from_utf8(recorded[0].body.clone()).expect("utf8");
        assert!(body.contains("grant_type=refresh_token"));
        assert!(body.contains("refresh_token=RT-OLD"));
    }

    #[test]
    fn token_endpoint_error_surfaces_oauth_error() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            "https://api.notion.com/v1/oauth/token",
            MockResponse {
                status: 400,
                headers: vec![("content-type".into(), "application/json".into())],
                body: br#"{"error":"invalid_grant","error_description":"code expired"}"#.to_vec(),
            },
        );
        let client = ReqwestOAuth2Client::new(transport).with_client_secret("s3cret");
        let err = client.exchange_code(&cfg(), "code").expect_err("must fail");
        match err {
            ConnectorError::Auth(msg) => {
                assert!(msg.contains("invalid_grant"));
                assert!(msg.contains("code expired"));
            }
            other => panic!("expected Auth, got {other:?}"),
        }
    }

    #[test]
    fn token_url_missing_fails() {
        let transport = Arc::new(MockHttpTransport::new());
        let client = ReqwestOAuth2Client::new(transport);
        let mut config = cfg();
        config.auth_config_json = serde_json::json!({});
        let err = client
            .exchange_code(&config, "c")
            .expect_err("missing token_url");
        match err {
            ConnectorError::Auth(msg) => assert!(msg.contains("token_url")),
            other => panic!("expected Auth, got {other:?}"),
        }
    }

    #[test]
    fn refresher_trait_impl_returns_error_without_config() {
        // The blanket TokenRefresher::refresh impl on
        // ReqwestOAuth2Client intentionally returns an error
        // pointing callers at refresh_with_config (where the
        // ConnectorConfig is available).
        let transport = Arc::new(MockHttpTransport::new());
        let client = ReqwestOAuth2Client::new(transport);
        let err = TokenRefresher::refresh(&client, "RT").expect_err("must fail");
        match err {
            ConnectorError::TokenRefresh(msg) => {
                assert!(msg.contains("refresh_with_config"));
            }
            other => panic!("expected TokenRefresh, got {other:?}"),
        }
    }

    #[test]
    fn encode_form_round_trip() {
        let encoded = encode_form(&[
            ("grant_type", "authorization_code"),
            ("code", "abc/123 xyz"),
            ("redirect_uri", "https://app.example.com/cb?a=1"),
        ]);
        assert_eq!(
            encoded,
            "grant_type=authorization_code&code=abc%2F123+xyz&redirect_uri=https%3A%2F%2Fapp.example.com%2Fcb%3Fa%3D1"
        );
    }
}
