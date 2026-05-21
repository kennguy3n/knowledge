//! Real OAuth2 client implementations.
//!
//! Provides transport-agnostic `OAuth2CodeExchange` and
//! `TokenRefresher` implementations that POST to a provider's
//! `/token` endpoint with `grant_type=authorization_code` and
//! `grant_type=refresh_token` respectively. The client is generic
//! over [`HttpTransport`] — the production binary plugs in the
//! `reqwest`-backed [`crate::http::BlockingHttpTransport`] (gated
//! behind the `http-client` feature flag so CI hosts without the
//! reqwest build chain can still compile the crate), while tests
//! swap in [`crate::http::MockHttpTransport`].
//!
//! The OAuth2 endpoint URL and `client_id` / `redirect_uri` are
//! read out of [`crate::config::ConnectorConfig::auth_config_json`]
//! (a flexible JSON blob — see `docs/DESIGN.md` §10.2). The
//! `client_secret` is **never** stored on disk; production callers
//! pass it in via [`OAuth2Client::with_client_secret`] at
//! runtime from the OS keychain / secrets manager.

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::Deserialize;

use crate::config::ConnectorConfig;
use crate::error::{ConnectorError, Result};
use crate::http::{HttpRequest, HttpResponse, HttpTransport};
use crate::token_vault::{
    OAuth2CodeExchange, OAuth2Token, RefreshedToken, SecretToken, TokenRefresher,
};

/// Transport-agnostic OAuth2 client that drives both
/// `authorization_code` and `refresh_token` grants over a shared
/// [`HttpTransport`].
///
/// The name was previously `ReqwestOAuth2Client`, but the type is
/// generic over [`HttpTransport`] and compiles without the `reqwest`
/// dependency — the only thing `reqwest`-specific is the default
/// [`crate::http::BlockingHttpTransport`] (behind the `http-client`
/// feature). Tests instantiate the client with
/// [`crate::http::MockHttpTransport`]; a future transport (e.g. a
/// `hyper`-only build, or a WASM `fetch`-backed transport) would
/// plug in here just as cleanly. A deprecated
/// [`ReqwestOAuth2Client`] alias remains for backward compatibility
/// — new code should use the [`OAuth2Client`] name.
///
/// One `OAuth2Client` instance services every connector
/// using the same transport — the per-provider knobs (token URL,
/// `client_id`, `client_secret`) come in via the
/// [`ConnectorConfig`] / the explicit `with_*` setters. The client
/// is `Clone` so callers can share it across the connector
/// runtime's worker pool.
/// `client_secret` is held as a [`SecretToken`] so its heap buffer
/// is zeroised on drop. The provider's OAuth2 client secret is a
/// long-lived, hard-to-rotate credential (rotating it requires
/// re-registering the application at the provider's developer
/// console), so leaving its bytes in freed memory after a drop or a
/// reqwest clone would leak it to whoever subsequently allocates
/// that heap region. `SecretToken` already gives the access /
/// refresh tokens the same treatment in [`OAuth2Token`] / [`RefreshedToken`]
/// — the client secret deserves *at least* the same care.
pub struct OAuth2Client<T: HttpTransport> {
    transport: std::sync::Arc<T>,
    client_secret: Option<SecretToken>,
}

// Manual `Clone` impl: a derived one would synthesise a `T: Clone`
// bound, but the only field that depends on `T` is `Arc<T>`, which
// is `Clone` *regardless* of whether `T` is. Cloning produces a
// second `Arc` handle to the shared transport (no underlying
// transport copy) and a `Clone` of the `Option<SecretToken>` (which
// is itself a fresh heap allocation that zeroises on drop).
impl<T: HttpTransport> Clone for OAuth2Client<T> {
    fn clone(&self) -> Self {
        Self {
            transport: std::sync::Arc::clone(&self.transport),
            client_secret: self.client_secret.clone(),
        }
    }
}

impl<T: HttpTransport> std::fmt::Debug for OAuth2Client<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuth2Client")
            .field("transport", &"<HttpTransport>")
            .field(
                "client_secret",
                &self.client_secret.as_ref().map(|_| "[redacted]"),
            )
            .finish()
    }
}

impl<T: HttpTransport> OAuth2Client<T> {
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
    ///
    /// The accepted `impl Into<String>` is wrapped in a [`SecretToken`]
    /// before being stored, so its heap buffer is zeroised on drop
    /// and a `Debug`-print of this client never exposes the value.
    /// Callers that already hold a [`SecretToken`] (e.g. surfaced
    /// from an OS keychain wrapper) can pass `secret.expose().to_owned()`
    /// here; the value re-enters a zeroising container immediately.
    #[must_use]
    pub fn with_client_secret(mut self, secret: impl Into<String>) -> Self {
        self.client_secret = Some(SecretToken::new(secret));
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

    /// POST a form-encoded body to a token endpoint and parse the
    /// response into a [`TokenResponse`].
    ///
    /// `error_kind` is the [`ConnectorError`] variant constructor
    /// used for non-2xx responses and invalid-JSON bodies. The two
    /// production grants use different variants:
    ///
    /// * `authorization_code` → [`ConnectorError::Auth`] (the call
    ///   site is interactive sign-in, so the host needs to surface a
    ///   re-auth prompt).
    /// * `refresh_token` → [`ConnectorError::TokenRefresh`] (the
    ///   call site is background token rotation; per the
    ///   [`TokenRefresher`] trait docs, refresher implementations
    ///   *must* return `TokenRefresh` on provider failure so callers
    ///   that pattern-match on the variant can trigger a full
    ///   re-authorisation flow rather than treating it as a generic
    ///   auth error).
    ///
    /// Transport-layer failures (connection reset, DNS, TLS) still
    /// surface as [`ConnectorError::Transport`] via the `?` on
    /// `self.transport.execute(req)` — they are never reclassified
    /// as `Auth` / `TokenRefresh`.
    fn execute_token_grant(
        &self,
        token_url: &str,
        form: &[(&str, &str)],
        error_kind: fn(String) -> ConnectorError,
    ) -> Result<TokenResponse> {
        // Manual urlencoded body — avoids pulling in a new
        // dependency just for OAuth2 form encoding.
        let body = encode_form(form);
        let req = HttpRequest::post(token_url, body.into_bytes())
            .with_header("Content-Type", "application/x-www-form-urlencoded")
            .with_header("Accept", "application/json");
        let resp = self.transport.execute(req)?;
        parse_token_response(&resp, error_kind)
    }
}

impl<T: HttpTransport + 'static> OAuth2CodeExchange for OAuth2Client<T> {
    fn exchange_code(&self, config: &ConnectorConfig, auth_code: &str) -> Result<OAuth2Token> {
        let token_url = Self::token_url(config)?;
        let client_id = Self::client_id(config)?;
        let redirect_uri = Self::redirect_uri(config)?;
        // `SecretToken::expose` is the explicit unwrap point — the
        // borrowed `&str` only lives long enough to feed the form
        // body builder below, and `expose` is documented as the
        // single read accessor (no `Deref<str>` impl exists, so we
        // cannot accidentally log it).
        //
        // When no `client_secret` has been configured we omit the
        // form field entirely rather than sending `client_secret=`
        // with an empty value — Azure AD (and some other strict
        // identity platforms) reject `invalid_client` for an empty
        // secret on a public-client (PKCE-style) registration. The
        // confidential-client flow used by Slack / Notion / Atlassian
        // / Google requires a non-empty secret, so the same omission
        // also surfaces a misconfigured confidential client as a
        // 400 from the provider rather than a 401 with a misleading
        // "empty client_secret" reason.
        let exposed_secret = self.client_secret.as_ref().map(SecretToken::expose);
        let mut form: Vec<(&str, &str)> = vec![
            ("grant_type", "authorization_code"),
            ("code", auth_code),
            ("redirect_uri", redirect_uri),
            ("client_id", client_id),
        ];
        if let Some(secret) = exposed_secret {
            form.push(("client_secret", secret));
        }
        let resp = self.execute_token_grant(token_url, &form, ConnectorError::Auth)?;
        Ok(token_response_to_oauth2(resp, Utc::now()))
    }
}

impl<T: HttpTransport + 'static> OAuth2Client<T> {
    /// Refresh an access token using `grant_type=refresh_token`.
    ///
    /// `OAuth2Client` deliberately does NOT implement
    /// [`TokenRefresher`] directly: the refresh grant always needs
    /// the per-connector [`ConnectorConfig`] (for the token URL and
    /// `client_id`), and a `TokenRefresher` impl whose `refresh` method
    /// silently ignored the config would be a footgun for
    /// `OAuth2TokenVault::refresh_if_expiring` callers. Use
    /// [`ConfiguredRefresher`] instead when a `TokenRefresher` is
    /// needed — it pairs this client with a config so the
    /// trait method has everything it needs.
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
        // See [`OAuth2CodeExchange::exchange_code`] for the rationale
        // on conditionally omitting `client_secret` rather than
        // sending an empty string — same constraint applies to the
        // refresh grant.
        let exposed_secret = self.client_secret.as_ref().map(SecretToken::expose);
        let mut form: Vec<(&str, &str)> = vec![
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", client_id),
        ];
        if let Some(secret) = exposed_secret {
            form.push(("client_secret", secret));
        }
        // Refresh-grant failures must surface as
        // `ConnectorError::TokenRefresh` per the [`TokenRefresher`]
        // trait contract — see the doc on `execute_token_grant`
        // above.
        let resp = self.execute_token_grant(token_url, &form, ConnectorError::TokenRefresh)?;
        Ok(token_response_to_refreshed(resp, Utc::now()))
    }
}

/// Deprecated alias for [`OAuth2Client`].
///
/// The previous name was misleading: the type is generic over
/// [`HttpTransport`] and compiles cleanly without the `reqwest`
/// dependency. The new name is transport-agnostic and lines up
/// with how the rest of the crate refers to OAuth2 surfaces
/// ([`OAuth2Token`], [`OAuth2CodeExchange`], …). External callers
/// can keep using `ReqwestOAuth2Client` for one minor cycle to
/// soften the migration; new code should prefer [`OAuth2Client`].
#[deprecated(
    since = "0.2.0",
    note = "renamed to `OAuth2Client` — the type is transport-agnostic and works with any \
            `HttpTransport`, not just the reqwest-backed default"
)]
pub type ReqwestOAuth2Client<T> = OAuth2Client<T>;

/// `TokenRefresher` adapter that pairs a [`OAuth2Client`] with
/// the [`ConnectorConfig`] needed to drive the refresh grant.
///
/// `OAuth2TokenVault::refresh_if_expiring` only accepts
/// `&dyn TokenRefresher`, but the refresh grant always needs the
/// connector's token URL and client id — there is no realistic
/// `TokenRefresher` impl that works without that context. Earlier
/// revisions of this crate exposed a blanket `TokenRefresher` impl on
/// `OAuth2Client` whose `refresh` method always returned an
/// error directing the caller to use `refresh_with_config`; that
/// satisfied the trait type-system contract but violated its semantic
/// contract — every `OAuth2TokenVault` refresh would silently fail at
/// runtime with no compile-time signal. `ConfiguredRefresher` fixes
/// the asymmetry by capturing the missing config at construction
/// time, so the vault's polymorphic call path works correctly.
pub struct ConfiguredRefresher<T: HttpTransport> {
    client: OAuth2Client<T>,
    config: ConnectorConfig,
}

// Manual `Clone` for the same reason as `OAuth2Client`:
// avoid synthesising an unnecessary `T: Clone` bound on the
// `HttpTransport` type parameter (the transport is shared via
// `Arc` inside the embedded client).
impl<T: HttpTransport> Clone for ConfiguredRefresher<T> {
    fn clone(&self) -> Self {
        Self {
            client: self.client.clone(),
            config: self.config.clone(),
        }
    }
}

impl<T: HttpTransport> std::fmt::Debug for ConfiguredRefresher<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfiguredRefresher")
            .field("client", &self.client)
            .field("kind", &self.config.kind)
            .field("scope_id", &self.config.scope_id)
            .finish()
    }
}

impl<T: HttpTransport + 'static> ConfiguredRefresher<T> {
    /// Build a refresher that always uses `config` when driving the
    /// refresh grant. Clone the underlying client + config once at
    /// the call site that knows both pieces.
    #[must_use]
    pub fn new(client: OAuth2Client<T>, config: ConnectorConfig) -> Self {
        Self { client, config }
    }

    /// Return a reference to the captured config so the caller can
    /// re-use it (e.g. to drive an `exchange_code` grant against the
    /// same provider).
    #[must_use]
    pub fn config(&self) -> &ConnectorConfig {
        &self.config
    }
}

impl<T: HttpTransport + 'static> TokenRefresher for ConfiguredRefresher<T> {
    fn refresh(&self, refresh_token: &str) -> Result<RefreshedToken> {
        self.client.refresh_with_config(&self.config, refresh_token)
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

/// Parse a token-endpoint response into a [`TokenResponse`].
///
/// `error_kind` decides which [`ConnectorError`] variant non-success
/// responses and invalid-JSON bodies map to — see
/// [`OAuth2Client::execute_token_grant`] for the rationale on
/// why the two grants pick different variants.
fn parse_token_response(
    resp: &HttpResponse,
    error_kind: fn(String) -> ConnectorError,
) -> Result<TokenResponse> {
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
        return Err(error_kind(format!(
            "OAuth2 token endpoint returned status {} — {}",
            resp.status, detail
        )));
    }
    serde_json::from_slice::<TokenResponse>(&resp.body)
        .map_err(|e| error_kind(format!("token response not valid JSON: {e}")))
}

fn token_response_to_oauth2(resp: TokenResponse, now: DateTime<Utc>) -> OAuth2Token {
    let expires_at = now + expires_in_to_duration(resp.expires_in);
    let scope = resp.scope.unwrap_or_default();
    // Map the optional `refresh_token` field onto the
    // `OAuth2Token::refresh_token` discriminant rather than forcing
    // `Some(SecretToken::new(""))`. Slack legacy and PKCE-only public
    // clients omit the field entirely; storing `None` lets
    // `OAuth2TokenVault::refresh_if_expiring` short-circuit with a
    // structured re-auth-required error instead of POSTing
    // `refresh_token=` to the provider and surfacing the resulting
    // `invalid_grant` rejection back to the host.
    let mut token = match resp.refresh_token {
        Some(rt) => OAuth2Token::new(resp.access_token, rt, expires_at, scope),
        None => OAuth2Token::new_without_refresh(resp.access_token, expires_at, scope),
    };
    if let Some(t) = resp.token_type {
        token.token_type = t;
    }
    token
}

fn token_response_to_refreshed(resp: TokenResponse, now: DateTime<Utc>) -> RefreshedToken {
    // Wrap both token fields in [`SecretToken`] so the provider's
    // plaintext access / refresh tokens never sit in a non-zeroising
    // heap allocation — see the doc on [`RefreshedToken`] for the
    // rationale on why this intermediate type holds the same
    // discipline as [`OAuth2Token`].
    RefreshedToken {
        access_token: SecretToken::new(resp.access_token),
        refresh_token: resp.refresh_token.map(SecretToken::new),
        expires_at: now + expires_in_to_duration(resp.expires_in),
        scope: resp.scope,
    }
}

fn expires_in_to_duration(expires_in: Option<i64>) -> ChronoDuration {
    // Providers that omit `expires_in` (e.g. Slack legacy tokens)
    // get a defensive 1-hour default — the vault will refresh as
    // needed.
    //
    // A negative `expires_in` (RFC-violating, but observed from
    // buggy / malicious providers) is clamped to zero. Without this
    // clamp the token would be parsed as "already-expired by
    // `|expires_in|` seconds", which still degrades safely (the vault
    // refreshes on first use), but the clamp makes the semantics
    // explicit: "no usable lifetime" instead of "valid for a negative
    // duration". Callers can rely on `expires_at >= now` for any
    // non-`None` response, which simplifies retry-budget accounting
    // in the connector runtime.
    let seconds = expires_in.unwrap_or(3600).max(0);
    ChronoDuration::seconds(seconds)
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
                out.push(c as char);
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
pub fn default_oauth_client() -> Result<OAuth2Client<crate::http::BlockingHttpTransport>> {
    let transport = std::sync::Arc::new(crate::http::BlockingHttpTransport::with_timeout(
        std::time::Duration::from_secs(DEFAULT_OAUTH_TIMEOUT_SECS),
    )?);
    Ok(OAuth2Client::new(transport))
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
        let client = OAuth2Client::new(transport.clone()).with_client_secret("s3cret");
        let token = client.exchange_code(&cfg(), "code-xyz").expect("exchange");
        assert_eq!(token.access_token.expose(), "AT");
        assert_eq!(
            token.refresh_token.as_ref().map(SecretToken::expose),
            Some("RT")
        );
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
        let client = OAuth2Client::new(transport.clone()).with_client_secret("s3cret");
        let refreshed = client
            .refresh_with_config(&cfg(), "RT-OLD")
            .expect("refresh");
        assert_eq!(refreshed.access_token.expose(), "NEW");
        assert_eq!(refreshed.scope.as_deref(), Some("read_content"));
        // Provider didn't return a new refresh_token — keeper.
        assert!(refreshed.refresh_token.is_none());

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
        let client = OAuth2Client::new(transport).with_client_secret("s3cret");
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
    fn refresh_grant_error_surfaces_token_refresh_variant() {
        // Per the `TokenRefresher` trait contract, provider rejections of a
        // refresh grant must surface as `ConnectorError::TokenRefresh` —
        // NOT `ConnectorError::Auth` — so callers that pattern-match on
        // the variant can trigger a full re-auth flow rather than treating
        // it as a generic auth error. The shared `parse_token_response`
        // helper is parametrised by an `error_kind` ctor specifically to
        // keep this invariant explicit.
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            "https://api.notion.com/v1/oauth/token",
            MockResponse {
                status: 400,
                headers: vec![("content-type".into(), "application/json".into())],
                body: br#"{"error":"invalid_grant","error_description":"refresh token revoked"}"#
                    .to_vec(),
            },
        );
        let client = OAuth2Client::new(transport).with_client_secret("s3cret");

        // Direct path through `refresh_with_config`.
        let err = client
            .refresh_with_config(&cfg(), "RT-OLD")
            .expect_err("refresh must fail on provider rejection");
        match err {
            ConnectorError::TokenRefresh(msg) => {
                assert!(msg.contains("invalid_grant"));
                assert!(msg.contains("refresh token revoked"));
            }
            other => panic!("expected TokenRefresh, got {other:?}"),
        }
    }

    #[test]
    fn refresh_grant_error_via_token_refresher_trait_surfaces_token_refresh_variant() {
        // Same invariant as above but driven through the `&dyn TokenRefresher`
        // path that `OAuth2TokenVault::refresh_if_expiring` takes — this
        // is the polymorphic call site whose callers most need the
        // variant discrimination.
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            "https://api.notion.com/v1/oauth/token",
            MockResponse {
                status: 401,
                headers: vec![("content-type".into(), "application/json".into())],
                body: br#"{"error":"invalid_client"}"#.to_vec(),
            },
        );
        let client = OAuth2Client::new(transport).with_client_secret("s3cret");
        let refresher = ConfiguredRefresher::new(client, cfg());
        let err = (&refresher as &dyn TokenRefresher)
            .refresh("RT-OLD")
            .expect_err("trait-driven refresh must fail");
        match err {
            ConnectorError::TokenRefresh(msg) => assert!(msg.contains("invalid_client")),
            other => panic!("expected TokenRefresh, got {other:?}"),
        }
    }

    /// Providers that omit `refresh_token` (Slack legacy, PKCE-only
    /// public clients) must round-trip through `exchange_code` and
    /// land as `OAuth2Token::refresh_token = None` — not as
    /// `Some(SecretToken::new(""))`. The empty-string variant would
    /// later cause `OAuth2TokenVault::refresh_if_expiring` to POST
    /// `refresh_token=` to the provider, which every compliant
    /// implementation rejects as `invalid_grant`.
    #[test]
    fn exchange_code_without_refresh_token_stores_none() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            "https://api.notion.com/v1/oauth/token",
            MockResponse::ok_json(
                br#"{"access_token":"AT","expires_in":3600,"scope":"read_content"}"#.to_vec(),
            ),
        );
        let client = OAuth2Client::new(transport).with_client_secret("s3cret");
        let token = client.exchange_code(&cfg(), "code-xyz").expect("exchange");
        assert!(
            token.refresh_token.is_none(),
            "provider omitted refresh_token; OAuth2Token must store None, not Some(empty)"
        );
    }

    /// Buggy or malicious providers that return a negative `expires_in`
    /// must not produce a token whose `expires_at` is "valid for a
    /// negative duration". `expires_in_to_duration` clamps to zero,
    /// making `expires_at == now` (i.e. already-expired, refresh on
    /// first use) — never `now - |expires_in|`.
    #[test]
    fn negative_expires_in_clamps_to_zero() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            "https://api.notion.com/v1/oauth/token",
            MockResponse::ok_json(
                br#"{"access_token":"AT","refresh_token":"RT","expires_in":-3600,"scope":"x"}"#
                    .to_vec(),
            ),
        );
        let client = OAuth2Client::new(transport).with_client_secret("s3cret");
        let before = Utc::now();
        let token = client.exchange_code(&cfg(), "code").expect("exchange");
        let after = Utc::now();
        // `expires_at` lands inside the [before, after] window — i.e.
        // ~now, not in the past by an hour.
        assert!(
            token.expires_at >= before && token.expires_at <= after,
            "negative expires_in not clamped: expires_at {:?} outside [{:?}, {:?}]",
            token.expires_at,
            before,
            after
        );
    }

    #[test]
    fn token_url_missing_fails() {
        let transport = Arc::new(MockHttpTransport::new());
        let client = OAuth2Client::new(transport);
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
    fn configured_refresher_drives_refresh_grant_via_trait() {
        // `ConfiguredRefresher` is what `OAuth2TokenVault::refresh_if_expiring`
        // accepts (`&dyn TokenRefresher`). It must thread the captured
        // `ConnectorConfig` into the underlying client and return the
        // refreshed token. This pins the architecturally-correct
        // wiring after we removed the broken blanket
        // `TokenRefresher for OAuth2Client` impl.
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            "https://api.notion.com/v1/oauth/token",
            MockResponse::ok_json(
                br#"{"access_token":"FRESH","expires_in":3600,"scope":"read_content"}"#.to_vec(),
            ),
        );
        let client = OAuth2Client::new(transport.clone()).with_client_secret("s3cret");
        let refresher = ConfiguredRefresher::new(client, cfg());

        // Call through the trait object — this is the path
        // `OAuth2TokenVault::refresh_if_expiring` takes.
        let refreshed: RefreshedToken = (&refresher as &dyn TokenRefresher)
            .refresh("RT-OLD")
            .expect("trait-driven refresh succeeds");
        assert_eq!(refreshed.access_token.expose(), "FRESH");
        assert_eq!(refreshed.scope.as_deref(), Some("read_content"));

        // Wire-level: the refresh grant went out with the captured
        // config's client_id and the secret from the client.
        let recorded = transport.recorded();
        assert_eq!(recorded.len(), 1);
        let body = String::from_utf8(recorded[0].body.clone()).expect("utf8");
        assert!(body.contains("grant_type=refresh_token"));
        assert!(body.contains("refresh_token=RT-OLD"));
        assert!(body.contains("client_id=client-abc"));
        assert!(body.contains("client_secret=s3cret"));
    }

    /// `client_secret` is held in a [`SecretToken`] so its heap
    /// buffer is zeroised on drop, its `Debug` is redacted, and a
    /// `Clone` of the client doesn't leave a second un-zeroising
    /// copy on the heap. Earlier revisions stored a plain `String`
    /// and leaked the secret to freed memory after each clone /
    /// drop. Pinning the Debug + on-wire behaviour here makes that
    /// regression impossible to land silently.
    #[test]
    fn client_secret_is_redacted_in_debug_and_survives_clone() {
        let transport = Arc::new(MockHttpTransport::new());
        let client = OAuth2Client::new(transport.clone()).with_client_secret("super-secret-value");

        // Debug must NOT contain the raw secret.
        let dbg = format!("{client:?}");
        assert!(
            !dbg.contains("super-secret-value"),
            "Debug leaked client_secret: {dbg}"
        );
        assert!(
            dbg.contains("[redacted]"),
            "Debug must show '[redacted]' instead of the secret: {dbg}"
        );

        // Cloning must preserve the secret (it's the entire point
        // of `Clone` on this type — workers share the client across
        // the connector runtime) but each copy still lives inside
        // a `SecretToken` so the heap buffer zeroises on drop.
        let cloned = client.clone();
        transport.expect(
            HttpMethod::Post,
            "https://api.notion.com/v1/oauth/token",
            MockResponse::ok_json(
                br#"{"access_token":"AT","refresh_token":"RT","expires_in":3600,"scope":"s","token_type":"Bearer"}"#.to_vec(),
            ),
        );
        let _ = cloned
            .exchange_code(&cfg(), "code")
            .expect("clone retains secret");
        let recorded = transport.recorded();
        let body = String::from_utf8(recorded[0].body.clone()).expect("utf8");
        assert!(
            body.contains("client_secret=super-secret-value"),
            "Clone must still POST the secret"
        );
    }

    /// When the client is built without a `client_secret`, the token
    /// endpoint must NOT receive `client_secret=` (empty string) in
    /// the form body — Azure AD and other strict identity platforms
    /// reject that as `invalid_client` on public-client / PKCE
    /// registrations. The field is omitted entirely from the form.
    #[test]
    fn client_secret_omitted_from_form_when_unset() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            "https://api.notion.com/v1/oauth/token",
            MockResponse::ok_json(
                br#"{"access_token":"AT","refresh_token":"RT","expires_in":3600,"scope":"s","token_type":"Bearer"}"#.to_vec(),
            ),
        );
        // Build a client with NO secret (the public-client / PKCE
        // case). `with_client_secret` is intentionally not called.
        let client = OAuth2Client::new(transport.clone());
        let _ = client
            .exchange_code(&cfg(), "code-xyz")
            .expect("public-client exchange should succeed");

        let recorded = transport.recorded();
        let body = String::from_utf8(recorded[0].body.clone()).expect("utf8");
        assert!(
            !body.contains("client_secret"),
            "form body must omit `client_secret` entirely when none configured, got {body}"
        );
        // The other required fields still ride the form.
        assert!(body.contains("grant_type=authorization_code"));
        assert!(body.contains("code=code-xyz"));
        assert!(body.contains("client_id=client-abc"));

        // Same invariant for the refresh-token grant.
        transport.expect(
            HttpMethod::Post,
            "https://api.notion.com/v1/oauth/token",
            MockResponse::ok_json(
                br#"{"access_token":"NEW","expires_in":7200,"scope":"s"}"#.to_vec(),
            ),
        );
        let _ = client
            .refresh_with_config(&cfg(), "RT-OLD")
            .expect("public-client refresh should succeed");
        let recorded = transport.recorded();
        let body = String::from_utf8(recorded[1].body.clone()).expect("utf8");
        assert!(
            !body.contains("client_secret"),
            "refresh-grant form body must omit `client_secret` entirely when none configured, got {body}"
        );
        assert!(body.contains("grant_type=refresh_token"));
        assert!(body.contains("refresh_token=RT-OLD"));
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
