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
//! `client_secret` is resolved per-grant through a three-layer
//! fallback ladder, in priority order:
//!
//! 1. **Host-supplied [`ClientSecretResolver`]** (production path).
//!    The host registers a resolver callback via the FFI surface;
//!    the framework consults it at grant-time with
//!    `(kind, scope_id, client_id)`. The secret stays in the host's
//!    OS keychain and never lives in the substrate's persisted state.
//!    This is the architecturally-correct production path because it
//!    keeps confidential credentials off disk (mitigating both
//!    at-rest theft and inadvertent backup-snapshot exposure).
//! 2. **`auth_config_json["client_secret"]`** (fallback for tests /
//!    dev hosts). When the resolver is unset OR returns `None`, the
//!    framework reads an optional `client_secret` string field out of
//!    [`ConnectorConfig::auth_config_json`]. **The framework's
//!    documented design intent is that secrets DO NOT live in
//!    `auth_config_json`** (see the doc comment on the field); this
//!    fallback exists strictly so test harnesses, single-tenant CLI
//!    hosts, and migration scripts can stand up the OAuth2 round-trip
//!    without the resolver FFI ceremony. Production hosts SHOULD
//!    register a resolver and leave this field absent.
//! 3. **Static [`OAuth2Client::with_client_secret`]** (legacy / test
//!    convenience). When neither layer above produces a secret, the
//!    framework falls back to the value optionally set on the client
//!    at construction time. The framework's existing unit tests use
//!    this path; new code should prefer the resolver.
//!
//! When all three layers come up empty the form field is omitted
//! entirely — public-client (PKCE-only) providers work as-is;
//! confidential-client providers reject the grant with
//! `invalid_client`, which is the actionable signal the host needs
//! to either register a resolver or thread the secret through
//! `auth_config_json`.

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::Deserialize;
use std::sync::{Arc, RwLock};

use crate::config::ConnectorConfig;
use crate::error::{ConnectorError, Result};
use crate::http::{HttpRequest, HttpResponse, HttpTransport};
use crate::token_vault::{
    OAuth2CodeExchange, OAuth2Token, RefreshedToken, SecretToken, TokenRefresher,
};

/// Host-supplied callback for resolving an OAuth2 `client_secret`
/// at grant-time.
///
/// The substrate consults the registered resolver before every
/// `authorization_code` / `refresh_token` grant; the resolver is
/// invoked with `(kind, scope_id, client_id)` and returns the
/// matching `client_secret` (or `None` if the host has no secret
/// for that combination, in which case the framework falls back to
/// [`ConnectorConfig::auth_config_json`]`["client_secret"]`).
///
/// The resolver MUST be cheap to call — it is consulted on the
/// runtime mutex's critical path in the FFI substrate, although
/// the framework itself does not hold any locks while calling it.
/// Concretely, the resolver is invoked on the thread that drives
/// the OAuth2 grant (the `sync_connector` worker or the host's
/// thread driving `authenticate_connector` /
/// `refresh_connector_token`). Implementations should resolve the
/// secret from an in-memory cache populated at startup rather than
/// hitting the OS keychain on every call.
///
/// # Multi-tenancy & multi-app
///
/// All three arguments are surfaced so hosts can disambiguate
/// across the dimensions OAuth2 deployments actually vary on:
///
/// * `kind`: which provider (`"notion"`, `"google_drive"`, etc.).
/// * `scope_id`: which tenant / workspace the connector instance is
///   bound to (Uuid string).
/// * `client_id`: which OAuth2 app at the provider (one tenant may
///   register multiple apps for different teams or use cases).
///
/// A typical host implementation indexes its secret store by
/// `(scope_id, client_id)` and ignores `kind` (which is implied by
/// `client_id`); a single-tenant CLI host indexes by `kind` alone.
///
/// # Returning `None`
///
/// Returning `None` is NOT an error — it signals "I don't have a
/// secret for that combination, please fall through to the
/// `auth_config_json` fallback." Implementations that want to
/// hard-fail an unknown-secret query should return `None` and let
/// the provider reject the grant with `invalid_client`; the
/// resulting `ConnectorError` surfaces to the host with the
/// actionable diagnostic.
pub trait ClientSecretResolver: Send + Sync {
    /// Resolve the `client_secret` for the given grant context.
    ///
    /// Returns `Some(secret)` when the host can produce a secret
    /// matching the `(kind, scope_id, client_id)` tuple; returns
    /// `None` to defer to the next layer of the framework's
    /// fallback ladder (see the module-level docs). An empty string
    /// is treated as an explicit "no-secret" choice and short-
    /// circuits the fallback layers — see
    /// [`OAuth2Client::client_secret_for`]'s rustdoc for the
    /// rationale.
    fn resolve(&self, kind: &str, scope_id: &str, client_id: &str) -> Option<String>;
}

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
    transport: Arc<T>,
    client_secret: Option<SecretToken>,
    /// Host-supplied dynamic resolver consulted before the static
    /// `client_secret` / `auth_config_json` fallback. Stored behind
    /// `Arc<RwLock<...>>` so the FFI substrate can hand out
    /// `Arc<OAuth2Client>` clones to every connector and still let
    /// the host swap or unset the resolver after `open_store` (e.g.
    /// on a keychain unlock event). Reads happen on every grant;
    /// writes happen once per host lifecycle — `RwLock` fits.
    resolver: Arc<RwLock<Option<Arc<dyn ClientSecretResolver>>>>,
}

// Manual `Clone` impl: a derived one would synthesise a `T: Clone`
// bound, but the only field that depends on `T` is `Arc<T>`, which
// is `Clone` *regardless* of whether `T` is. Cloning produces a
// second `Arc` handle to the shared transport (no underlying
// transport copy), a `Clone` of the `Option<SecretToken>` (which is
// itself a fresh heap allocation that zeroises on drop), and a
// second `Arc` handle to the shared resolver slot (so clones
// observe `set_resolver` / `clear_resolver` calls).
impl<T: HttpTransport> Clone for OAuth2Client<T> {
    fn clone(&self) -> Self {
        Self {
            transport: Arc::clone(&self.transport),
            client_secret: self.client_secret.clone(),
            resolver: Arc::clone(&self.resolver),
        }
    }
}

impl<T: HttpTransport> std::fmt::Debug for OAuth2Client<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Snapshot the resolver behind a non-blocking read for the
        // diagnostic Debug print. A poisoned lock degrades to the
        // string "<poisoned>" — we deliberately don't propagate the
        // poison because Debug must be infallible and the only
        // poisoning vector (a panic inside set_resolver while the
        // write lock was held) is already a hard failure visible
        // elsewhere.
        let resolver_state = match self.resolver.read() {
            Ok(guard) => {
                if guard.is_some() {
                    "<registered>"
                } else {
                    "<unset>"
                }
            }
            Err(_) => "<poisoned>",
        };
        f.debug_struct("OAuth2Client")
            .field("transport", &"<HttpTransport>")
            .field(
                "client_secret",
                &self.client_secret.as_ref().map(|_| "[redacted]"),
            )
            .field("resolver", &resolver_state)
            .finish()
    }
}

impl<T: HttpTransport> OAuth2Client<T> {
    /// Wrap a transport. No client secret is set — callers that
    /// negotiate against providers requiring a confidential client
    /// (most major providers: Google, Microsoft, Atlassian, …)
    /// must either:
    ///
    /// 1. Register a host-supplied [`ClientSecretResolver`] via
    ///    [`Self::set_resolver`] (recommended for production —
    ///    secrets stay in the OS keychain).
    /// 2. Pass `client_secret` through
    ///    [`ConnectorConfig::auth_config_json`] (fallback for tests
    ///    / dev hosts only — see the module-level docs for the
    ///    rationale).
    /// 3. Call [`Self::with_client_secret`] (legacy / unit-test
    ///    convenience; the value lives in this client's memory).
    pub fn new(transport: Arc<T>) -> Self {
        Self {
            transport,
            client_secret: None,
            resolver: Arc::new(RwLock::new(None)),
        }
    }

    /// Register a host-supplied resolver for `client_secret` lookup.
    ///
    /// The resolver is consulted on every `authorization_code` /
    /// `refresh_token` grant before the `auth_config_json` and
    /// static fallbacks. Calling this method more than once
    /// REPLACES the previously-registered resolver — the framework
    /// holds at most one resolver per `OAuth2Client` instance,
    /// shared by every clone of the client through an internal
    /// `Arc<RwLock<...>>`.
    ///
    /// Takes `&self` (not `&mut self`) so the FFI substrate can
    /// register a resolver against the `Arc<OAuth2Client>` it
    /// already shares with every connector — no need to rebuild
    /// the per-runtime client or re-bind the per-connector
    /// `Arc<dyn OAuth2CodeExchange>` references.
    ///
    /// # Panics
    ///
    /// Panics if the internal `RwLock` is poisoned (i.e. a previous
    /// caller of this method panicked while holding the write lock).
    /// In practice this is impossible — the body of this method
    /// performs a single assignment and cannot panic — but the
    /// stdlib's `RwLock::write` returns `Result` so we surface the
    /// `expect`. A poisoned `RwLock` is a programming bug, not a
    /// runtime condition the host can recover from.
    pub fn set_resolver(&self, resolver: Arc<dyn ClientSecretResolver>) {
        let mut guard = self
            .resolver
            .write()
            .expect("OAuth2Client resolver RwLock poisoned");
        *guard = Some(resolver);
    }

    /// Unregister any previously-registered resolver. After this
    /// call the framework consults `auth_config_json` and the
    /// static `with_client_secret` value only.
    ///
    /// # Panics
    ///
    /// Same as [`Self::set_resolver`] — `RwLock` poisoning.
    pub fn clear_resolver(&self) {
        let mut guard = self
            .resolver
            .write()
            .expect("OAuth2Client resolver RwLock poisoned");
        *guard = None;
    }

    /// Resolve the `client_secret` for a grant against `config`,
    /// walking the 3-layer fallback ladder documented at the
    /// module level. Returns `None` when no layer produces a
    /// secret (the framework then omits the `client_secret` form
    /// field).
    ///
    /// Layer 1 (resolver) holds the read lock for the duration of
    /// the resolver call. The trait contract documents that
    /// implementations must be cheap (in-memory cache lookups), so
    /// the lock is never held across a long-blocking operation. A
    /// poisoned lock degrades cleanly to layer 2 (auth_config_json)
    /// — the framework keeps trying to make progress rather than
    /// surfacing a `RwLockReadGuard` poison to the host as an OAuth2
    /// error.
    fn client_secret_for(&self, config: &ConnectorConfig) -> Option<String> {
        // Layer 1: host-supplied resolver.
        if let Ok(guard) = self.resolver.read() {
            if let Some(resolver) = guard.as_ref() {
                let kind = config.kind.as_str();
                let scope_id = config.scope_id.as_uuid().to_string();
                let client_id = config
                    .auth_config_json
                    .get("client_id")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                if let Some(secret) = resolver.resolve(kind, &scope_id, client_id) {
                    if !secret.is_empty() {
                        return Some(secret);
                    }
                    // Empty string from resolver is treated as
                    // "explicit no-secret" and short-circuits the
                    // fallback layers — the host has affirmatively
                    // chosen the public-client form for this
                    // (kind, scope_id, client_id) tuple, and falling
                    // back to `auth_config_json` would produce
                    // confusing precedence semantics.
                    return None;
                }
                // Resolver returned None → fall through to layer 2.
            }
        }
        // Layer 2: auth_config_json["client_secret"] (fallback).
        if let Some(secret) = config
            .auth_config_json
            .get("client_secret")
            .and_then(serde_json::Value::as_str)
        {
            if !secret.is_empty() {
                return Some(secret.to_string());
            }
        }
        // Layer 3: static client_secret set via with_client_secret.
        self.client_secret.as_ref().map(|s| s.expose().to_string())
    }

    /// Provide the OAuth2 client secret (kept in memory only — the
    /// substrate never persists it). Chainable.
    ///
    /// **Layer-3 fallback only** — the framework's documented
    /// production path is the host-supplied
    /// [`ClientSecretResolver`] registered via [`Self::set_resolver`]
    /// (see the module-level docs). This method is retained for
    /// backwards compatibility with code that constructs an
    /// `OAuth2Client` with a known secret at build time (chiefly
    /// unit-test harnesses), and for hosts that want a single
    /// fallback secret applied across every grant when neither the
    /// resolver nor `auth_config_json` produces one.
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
        // Resolve `client_secret` through the 3-layer ladder
        // documented on [`Self::client_secret_for`]:
        // 1. host-supplied resolver, 2. `auth_config_json`, 3.
        // static. When all three come up empty the form field is
        // omitted entirely rather than sent with an empty value —
        // Azure AD (and some other strict identity platforms) reject
        // `invalid_client` for an empty secret on a public-client
        // (PKCE-style) registration. The confidential-client flow
        // used by Slack / Notion / Atlassian / Google requires a
        // non-empty secret, so the same omission also surfaces a
        // misconfigured confidential client as a 400 from the
        // provider rather than a 401 with a misleading "empty
        // client_secret" reason.
        let resolved_secret = self.client_secret_for(config);
        let mut form: Vec<(&str, &str)> = vec![
            ("grant_type", "authorization_code"),
            ("code", auth_code),
            ("redirect_uri", redirect_uri),
            ("client_id", client_id),
        ];
        if let Some(secret) = resolved_secret.as_deref() {
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
        // on the 3-layer resolution ladder and the omit-on-empty
        // convention — same constraint applies to the refresh grant.
        let resolved_secret = self.client_secret_for(config);
        let mut form: Vec<(&str, &str)> = vec![
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", client_id),
        ];
        if let Some(secret) = resolved_secret.as_deref() {
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

    // ───────── Phase 4.1: ClientSecretResolver resolution-ladder tests ─────────

    /// Test resolver that records every `(kind, scope_id, client_id)`
    /// tuple it's asked about and returns a preset answer. Mirrors the
    /// `MockHttpTransport` recording pattern in this module.
    #[derive(Debug, Default)]
    struct RecordingResolver {
        answer: std::sync::Mutex<Option<String>>,
        calls: std::sync::Mutex<Vec<(String, String, String)>>,
    }

    impl RecordingResolver {
        fn with_answer(answer: impl Into<String>) -> Self {
            Self {
                answer: std::sync::Mutex::new(Some(answer.into())),
                calls: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn no_answer() -> Self {
            Self::default()
        }

        fn calls(&self) -> Vec<(String, String, String)> {
            self.calls.lock().expect("calls poisoned").clone()
        }
    }

    impl ClientSecretResolver for RecordingResolver {
        fn resolve(&self, kind: &str, scope_id: &str, client_id: &str) -> Option<String> {
            self.calls.lock().expect("calls poisoned").push((
                kind.to_string(),
                scope_id.to_string(),
                client_id.to_string(),
            ));
            self.answer.lock().expect("answer poisoned").clone()
        }
    }

    /// Layer 1: a registered resolver that returns `Some(secret)` MUST
    /// supply the value to the grant body — the framework never falls
    /// through to `auth_config_json` or the static client secret when
    /// the resolver has answered.
    #[test]
    fn resolver_secret_overrides_auth_config_json_and_static_layers() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            "https://api.notion.com/v1/oauth/token",
            MockResponse::ok_json(
                br#"{"access_token":"AT","refresh_token":"RT","expires_in":3600,"scope":"s"}"#
                    .to_vec(),
            ),
        );
        // Build a client with a STATIC secret (layer 3) AND configure
        // a resolver (layer 1). Resolver must win.
        let client = OAuth2Client::new(transport.clone()).with_client_secret("static-secret");
        let resolver = Arc::new(RecordingResolver::with_answer("resolver-secret"));
        client.set_resolver(resolver.clone());

        // Also stash a layer-2 secret in auth_config_json. Resolver
        // still wins over both layers below it.
        let mut config = cfg();
        config.auth_config_json["client_secret"] = serde_json::json!("auth-config-secret");

        let _ = client
            .exchange_code(&config, "code-xyz")
            .expect("exchange succeeds");

        let body = String::from_utf8(transport.recorded()[0].body.clone()).expect("utf8");
        assert!(
            body.contains("client_secret=resolver-secret"),
            "resolver must override both auth_config_json and static layers; got {body}"
        );
        assert!(
            !body.contains("static-secret"),
            "static client_secret must be shadowed by resolver"
        );
        assert!(
            !body.contains("auth-config-secret"),
            "auth_config_json client_secret must be shadowed by resolver"
        );

        // Resolver received the right context tuple.
        let calls = resolver.calls();
        assert_eq!(calls.len(), 1);
        let (kind, scope_id, client_id) = &calls[0];
        assert_eq!(kind, "notion");
        assert_eq!(scope_id, &config.scope_id.as_uuid().to_string());
        assert_eq!(client_id, "client-abc");
    }

    /// Layer 2 fallback: when the resolver returns `None`, the
    /// framework falls through to `auth_config_json["client_secret"]`.
    /// This is the dev/test/CLI ergonomic path.
    #[test]
    fn auth_config_json_client_secret_used_when_resolver_returns_none() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            "https://api.notion.com/v1/oauth/token",
            MockResponse::ok_json(
                br#"{"access_token":"AT","expires_in":3600,"scope":"s"}"#.to_vec(),
            ),
        );
        let client = OAuth2Client::new(transport.clone());
        let resolver = Arc::new(RecordingResolver::no_answer());
        client.set_resolver(resolver.clone());

        let mut config = cfg();
        config.auth_config_json["client_secret"] = serde_json::json!("from-auth-config");

        let _ = client
            .refresh_with_config(&config, "RT-OLD")
            .expect("refresh succeeds");

        let body = String::from_utf8(transport.recorded()[0].body.clone()).expect("utf8");
        assert!(
            body.contains("client_secret=from-auth-config"),
            "expected layer-2 fallback to populate client_secret; got {body}"
        );
        // Resolver was consulted (we don't skip it).
        assert_eq!(resolver.calls().len(), 1);
    }

    /// Layer 2 also kicks in when no resolver is registered AT ALL
    /// (not just when the resolver returns `None`). This is the
    /// most common test-harness invocation.
    #[test]
    fn auth_config_json_client_secret_used_when_no_resolver_registered() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            "https://api.notion.com/v1/oauth/token",
            MockResponse::ok_json(
                br#"{"access_token":"AT","refresh_token":"RT","expires_in":3600,"scope":"s"}"#
                    .to_vec(),
            ),
        );
        let client = OAuth2Client::new(transport.clone());

        let mut config = cfg();
        config.auth_config_json["client_secret"] = serde_json::json!("ac-only");

        let _ = client
            .exchange_code(&config, "code")
            .expect("exchange succeeds");

        let body = String::from_utf8(transport.recorded()[0].body.clone()).expect("utf8");
        assert!(
            body.contains("client_secret=ac-only"),
            "auth_config_json fallback must fire when no resolver registered; got {body}"
        );
    }

    /// Layer 3 fallback: when both the resolver and `auth_config_json`
    /// come up empty, the static `with_client_secret` value wins.
    /// Preserves backwards compatibility with existing unit-test
    /// wiring that constructed `OAuth2Client::new(...).with_client_secret(...)`.
    #[test]
    fn static_client_secret_used_when_resolver_and_auth_config_both_empty() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            "https://api.notion.com/v1/oauth/token",
            MockResponse::ok_json(
                br#"{"access_token":"AT","refresh_token":"RT","expires_in":3600,"scope":"s"}"#
                    .to_vec(),
            ),
        );
        let client = OAuth2Client::new(transport.clone()).with_client_secret("static-only");
        let resolver = Arc::new(RecordingResolver::no_answer());
        client.set_resolver(resolver);

        // No `client_secret` in auth_config_json.
        let _ = client
            .exchange_code(&cfg(), "code")
            .expect("exchange succeeds");

        let body = String::from_utf8(transport.recorded()[0].body.clone()).expect("utf8");
        assert!(
            body.contains("client_secret=static-only"),
            "static layer must catch when both higher layers come up empty; got {body}"
        );
    }

    /// Empty string from the resolver is treated as "explicit
    /// public-client, omit the form field"; it short-circuits the
    /// lower layers rather than falling through. Documents the
    /// precedence rule in `client_secret_for`.
    #[test]
    fn resolver_returning_empty_string_short_circuits_fallback() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            "https://api.notion.com/v1/oauth/token",
            MockResponse::ok_json(
                br#"{"access_token":"AT","expires_in":3600,"scope":"s"}"#.to_vec(),
            ),
        );
        let client = OAuth2Client::new(transport.clone()).with_client_secret("static-fallback");
        let resolver = Arc::new(RecordingResolver::with_answer(""));
        client.set_resolver(resolver);

        let mut config = cfg();
        config.auth_config_json["client_secret"] = serde_json::json!("ac-fallback");

        let _ = client
            .exchange_code(&config, "code")
            .expect("exchange succeeds");

        let body = String::from_utf8(transport.recorded()[0].body.clone()).expect("utf8");
        assert!(
            !body.contains("client_secret"),
            "empty-string resolver answer must omit client_secret entirely, not fall through; got {body}"
        );
    }

    /// `set_resolver` followed by `clear_resolver` returns the client
    /// to the fallback-only state. Pins the unregister path.
    #[test]
    fn clear_resolver_falls_back_to_auth_config_json() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            "https://api.notion.com/v1/oauth/token",
            MockResponse::ok_json(
                br#"{"access_token":"AT","refresh_token":"RT","expires_in":3600,"scope":"s"}"#
                    .to_vec(),
            ),
        );
        let client = OAuth2Client::new(transport.clone());
        client.set_resolver(Arc::new(RecordingResolver::with_answer("from-resolver")));
        client.clear_resolver();

        let mut config = cfg();
        config.auth_config_json["client_secret"] = serde_json::json!("from-auth-config");

        let _ = client
            .exchange_code(&config, "code")
            .expect("exchange succeeds");

        let body = String::from_utf8(transport.recorded()[0].body.clone()).expect("utf8");
        assert!(
            body.contains("client_secret=from-auth-config"),
            "after clear_resolver(), auth_config_json must take over; got {body}"
        );
    }

    /// `Clone` of an `OAuth2Client` shares the resolver slot with the
    /// original — registering a resolver on one clone changes the
    /// behaviour of every other clone. This is the invariant the FFI
    /// substrate relies on: it hands an `Arc<OAuth2Client>` to every
    /// connector, then calls `set_resolver` once on the runtime's
    /// canonical handle, and every connector observes the new
    /// resolver on the next grant.
    #[test]
    fn resolver_is_shared_across_clones() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            "https://api.notion.com/v1/oauth/token",
            MockResponse::ok_json(
                br#"{"access_token":"AT","refresh_token":"RT","expires_in":3600,"scope":"s"}"#
                    .to_vec(),
            ),
        );
        let client_a = OAuth2Client::new(transport.clone());
        let client_b = client_a.clone();

        // Register resolver on `client_a`; expect `client_b` to see it.
        client_a.set_resolver(Arc::new(RecordingResolver::with_answer("shared-secret")));

        let _ = client_b
            .exchange_code(&cfg(), "code")
            .expect("clone-driven grant succeeds");

        let body = String::from_utf8(transport.recorded()[0].body.clone()).expect("utf8");
        assert!(
            body.contains("client_secret=shared-secret"),
            "Clone of OAuth2Client must share the resolver slot; got {body}"
        );
    }

    /// `Debug` output indicates whether a resolver is registered but
    /// MUST NOT call into the resolver (which could panic / block /
    /// log the secret). Pins the "Debug is infallible and quiet"
    /// invariant.
    #[test]
    fn debug_indicates_resolver_state_without_invoking_it() {
        let transport = Arc::new(MockHttpTransport::new());
        let client = OAuth2Client::new(transport);

        // Unset state.
        let dbg_unset = format!("{client:?}");
        assert!(
            dbg_unset.contains("<unset>"),
            "Debug must indicate unset resolver; got {dbg_unset}"
        );

        // After registering a resolver that would panic if called.
        #[derive(Debug)]
        struct PanickingResolver;
        impl ClientSecretResolver for PanickingResolver {
            fn resolve(&self, _: &str, _: &str, _: &str) -> Option<String> {
                panic!("Debug must not invoke the resolver");
            }
        }
        client.set_resolver(Arc::new(PanickingResolver));
        let dbg_set = format!("{client:?}");
        assert!(
            dbg_set.contains("<registered>"),
            "Debug must indicate registered resolver; got {dbg_set}"
        );
    }
}
