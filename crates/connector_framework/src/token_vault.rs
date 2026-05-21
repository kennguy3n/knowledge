//! OAuth2 token vault.
//!
//! Stores per-connector-instance OAuth2 tokens (access + refresh +
//! expiry) and exposes a refresh flow. Tokens at rest are wrapped in
//! [`SecretToken`] which zeroises on drop; the in-memory vault is
//! the baseline (the production substrate will swap in an
//! SQLCipher-backed table behind the same surface).

use std::collections::HashMap;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::error::{ConnectorError, Result};

/// Identifier for one running connector instance — opaque UUID v4.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ConnectorInstanceId(pub Uuid);

impl ConnectorInstanceId {
    /// Generate a fresh id.
    pub fn new_v4() -> Self {
        Self(Uuid::new_v4())
    }

    /// Wrap a raw [`Uuid`].
    pub fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid)
    }

    /// Borrow the underlying [`Uuid`].
    pub fn as_uuid(&self) -> Uuid {
        self.0
    }
}

impl std::fmt::Display for ConnectorInstanceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// A secret token wrapper that zeroises on drop. Equivalent to a
/// `Box<str>` in storage but never logged via its `Debug` impl.
#[derive(Clone, Zeroize, ZeroizeOnDrop, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SecretToken(String);

impl SecretToken {
    /// Wrap a token string.
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// Borrow the underlying secret. Callers must not log the result.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for SecretToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("SecretToken").field(&"[redacted]").finish()
    }
}

/// One OAuth2 token bundle for a connector instance.
///
/// `refresh_token` is intentionally an [`Option`]: some providers
/// (Notion, Atlassian rotating-token mode) always return a refresh
/// token, while others (Slack legacy, public-client / PKCE-only
/// flows, single-page-app flows that opt out of refresh rotation)
/// never do. Storing `None` for the latter is more honest than
/// faking an empty `SecretToken`: it lets
/// [`OAuth2TokenVault::refresh_if_expiring`] short-circuit with a
/// structured [`ConnectorError::TokenRefresh`] instead of attempting
/// a refresh grant with `refresh_token=`, which every compliant
/// provider rejects as `invalid_grant`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuth2Token {
    /// Bearer access token.
    pub access_token: SecretToken,
    /// Refresh token (long-lived, used to mint a new access token
    /// when the current one expires). `None` when the provider did
    /// not issue one — the vault treats this as "re-authorisation
    /// required at next refresh" rather than attempting a doomed
    /// refresh grant with an empty token.
    pub refresh_token: Option<SecretToken>,
    /// Wall-clock expiration of `access_token`.
    pub expires_at: DateTime<Utc>,
    /// OAuth2 scope string as granted by the provider.
    pub scope: String,
    /// Token type (typically `"Bearer"`).
    pub token_type: String,
}

impl OAuth2Token {
    /// Construct a new token bundle with both access and refresh
    /// tokens — the common case for confidential-client flows.
    pub fn new(
        access_token: impl Into<String>,
        refresh_token: impl Into<String>,
        expires_at: DateTime<Utc>,
        scope: impl Into<String>,
    ) -> Self {
        Self {
            access_token: SecretToken::new(access_token),
            refresh_token: Some(SecretToken::new(refresh_token)),
            expires_at,
            scope: scope.into(),
            token_type: "Bearer".to_string(),
        }
    }

    /// Construct a new token bundle *without* a refresh token —
    /// callers should use this when the provider explicitly omits
    /// `refresh_token` from the token response (Slack legacy,
    /// PKCE-only public clients, etc.) rather than passing an empty
    /// string to [`Self::new`]. The vault uses the `None` discriminant
    /// to surface a structured re-auth-required error instead of
    /// attempting an empty-token refresh grant.
    pub fn new_without_refresh(
        access_token: impl Into<String>,
        expires_at: DateTime<Utc>,
        scope: impl Into<String>,
    ) -> Self {
        Self {
            access_token: SecretToken::new(access_token),
            refresh_token: None,
            expires_at,
            scope: scope.into(),
            token_type: "Bearer".to_string(),
        }
    }

    /// True iff `now` is at or past [`Self::expires_at`].
    pub fn is_expired(&self, now: DateTime<Utc>) -> bool {
        now >= self.expires_at
    }

    /// True iff `now` is within `skew` of [`Self::expires_at`] (i.e.
    /// the token is "expiring soon" and a refresh is recommended).
    pub fn is_expiring_within(&self, now: DateTime<Utc>, skew: Duration) -> bool {
        now + skew >= self.expires_at
    }
}

/// Result of a token-refresh flow — the *new* access/refresh pair
/// returned by the provider's token endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshedToken {
    /// New access token.
    pub access_token: String,
    /// New refresh token (some providers rotate, others return the
    /// same value). Pass `None` to keep the existing refresh token.
    pub refresh_token: Option<String>,
    /// New expiry.
    pub expires_at: DateTime<Utc>,
    /// New scope (if changed).
    pub scope: Option<String>,
}

/// Pluggable token-refresh hook — invoked by [`OAuth2TokenVault`]
/// when a stored token is at or past its expiry.
///
/// Production code wires this to an HTTP client that calls the
/// provider's `/token` endpoint with `grant_type=refresh_token`. The
/// trait keeps the vault provider-agnostic and unit-testable.
pub trait TokenRefresher {
    /// Exchange `refresh_token` for a new access token. Implementors
    /// should return [`ConnectorError::TokenRefresh`] on failure.
    fn refresh(&self, refresh_token: &str) -> Result<RefreshedToken>;
}

/// One-shot OAuth2 authorisation-code exchange — invoked by a
/// connector's `authenticate` method when the substrate first
/// negotiates access.
///
/// Production code wires this to an HTTP client that POSTs to the
/// provider's `/token` endpoint with
/// `grant_type=authorization_code`. The trait keeps the connector
/// boundary provider-agnostic.
pub trait OAuth2CodeExchange {
    /// Exchange `auth_code` for an access / refresh token pair.
    /// `redirect_uri` and `client_id` come from
    /// `config.auth_config_json` (see [`crate::config::ConnectorConfig`]).
    ///
    /// # Errors
    ///
    /// Returns [`ConnectorError::Auth`] on provider rejection or
    /// [`ConnectorError::Transport`] on network failure.
    fn exchange_code(
        &self,
        config: &crate::config::ConnectorConfig,
        auth_code: &str,
    ) -> Result<OAuth2Token>;
}

/// In-memory OAuth2 token vault, keyed by [`ConnectorInstanceId`].
///
/// The vault stores [`OAuth2Token`] bundles per connector instance
/// and offers a [`Self::refresh_if_expiring`] helper that drives a
/// [`TokenRefresher`] when the cached token is at or near expiry.
#[derive(Debug, Clone, Default)]
pub struct OAuth2TokenVault {
    tokens: HashMap<ConnectorInstanceId, OAuth2Token>,
    /// Default skew applied by [`Self::refresh_if_expiring`].
    default_skew: Duration,
}

impl OAuth2TokenVault {
    /// Construct an empty vault.
    pub fn new() -> Self {
        Self {
            tokens: HashMap::new(),
            default_skew: Duration::seconds(60),
        }
    }

    /// Override the default refresh skew (the window before expiry
    /// at which `refresh_if_expiring` proactively refreshes).
    pub fn with_default_skew(mut self, skew: Duration) -> Self {
        self.default_skew = skew;
        self
    }

    /// Number of stored tokens.
    pub fn len(&self) -> usize {
        self.tokens.len()
    }

    /// True iff no tokens are stored.
    pub fn is_empty(&self) -> bool {
        self.tokens.is_empty()
    }

    /// Insert or replace the token bundle for `instance`.
    pub fn put(&mut self, instance: ConnectorInstanceId, token: OAuth2Token) {
        self.tokens.insert(instance, token);
    }

    /// Borrow the token for `instance`, returning
    /// [`ConnectorError::TokenNotFound`] if missing.
    pub fn get(&self, instance: ConnectorInstanceId) -> Result<&OAuth2Token> {
        self.tokens
            .get(&instance)
            .ok_or(ConnectorError::TokenNotFound)
    }

    /// Remove the token for `instance`, returning
    /// [`ConnectorError::TokenNotFound`] if missing.
    pub fn remove(&mut self, instance: ConnectorInstanceId) -> Result<OAuth2Token> {
        self.tokens
            .remove(&instance)
            .ok_or(ConnectorError::TokenNotFound)
    }

    /// Refresh the token for `instance` if `now + skew >= expires_at`.
    ///
    /// On a successful refresh, the vault is updated in place and a
    /// reference to the new token is returned. If the token is not
    /// expiring soon, the existing token is returned unchanged.
    ///
    /// `skew` defaults to [`Self::default_skew`] when `None`.
    pub fn refresh_if_expiring(
        &mut self,
        instance: ConnectorInstanceId,
        now: DateTime<Utc>,
        skew: Option<Duration>,
        refresher: &dyn TokenRefresher,
    ) -> Result<&OAuth2Token> {
        let skew = skew.unwrap_or(self.default_skew);
        let needs_refresh = self
            .tokens
            .get(&instance)
            .ok_or(ConnectorError::TokenNotFound)?
            .is_expiring_within(now, skew);
        if needs_refresh {
            // If the stored token has no refresh token (provider
            // never issued one), refusing to call the refresher is
            // strictly better than passing an empty string: every
            // OAuth2 server rejects `refresh_token=` as
            // `invalid_grant`, and the resulting `TokenRefresh`
            // error message would be misleading ("provider rejected
            // our empty refresh token" rather than "we never had a
            // refresh token in the first place"). Hosts switch on
            // `ConnectorError::TokenRefresh` to drive re-authorisation
            // UI, so the variant is correct.
            let refresh_token = self
                .tokens
                .get(&instance)
                .expect("checked above")
                .refresh_token
                .as_ref()
                .ok_or_else(|| {
                    ConnectorError::TokenRefresh(
                        "cannot refresh: no refresh_token stored for connector instance — \
                         re-authorisation required"
                            .into(),
                    )
                })?
                .expose()
                .to_string();
            let refreshed = refresher.refresh(&refresh_token)?;
            let entry = self.tokens.get_mut(&instance).expect("checked above");
            entry.access_token = SecretToken::new(refreshed.access_token);
            if let Some(rt) = refreshed.refresh_token {
                entry.refresh_token = Some(SecretToken::new(rt));
            }
            entry.expires_at = refreshed.expires_at;
            if let Some(s) = refreshed.scope {
                entry.scope = s;
            }
        }
        Ok(self.tokens.get(&instance).expect("present"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StaticRefresher {
        access: String,
        refresh: Option<String>,
        expires_at: DateTime<Utc>,
    }

    impl TokenRefresher for StaticRefresher {
        fn refresh(&self, _refresh_token: &str) -> Result<RefreshedToken> {
            Ok(RefreshedToken {
                access_token: self.access.clone(),
                refresh_token: self.refresh.clone(),
                expires_at: self.expires_at,
                scope: None,
            })
        }
    }

    struct FailingRefresher;
    impl TokenRefresher for FailingRefresher {
        fn refresh(&self, _refresh_token: &str) -> Result<RefreshedToken> {
            Err(ConnectorError::TokenRefresh("provider rejected".into()))
        }
    }

    #[test]
    fn put_and_get_round_trip() {
        let mut vault = OAuth2TokenVault::new();
        let id = ConnectorInstanceId::new_v4();
        let tok = OAuth2Token::new(
            "access-1",
            "refresh-1",
            Utc::now() + Duration::hours(1),
            "drive.read",
        );
        vault.put(id, tok.clone());
        assert_eq!(vault.get(id).unwrap(), &tok);
    }

    #[test]
    fn missing_token_errors() {
        let vault = OAuth2TokenVault::new();
        let err = vault.get(ConnectorInstanceId::new_v4()).unwrap_err();
        assert!(matches!(err, ConnectorError::TokenNotFound));
    }

    #[test]
    fn refresh_runs_when_expired() {
        let mut vault = OAuth2TokenVault::new();
        let id = ConnectorInstanceId::new_v4();
        let now = Utc::now();
        let expired = OAuth2Token::new(
            "old-access",
            "old-refresh",
            now - Duration::seconds(5),
            "scope",
        );
        vault.put(id, expired);
        let new_expiry = now + Duration::hours(1);
        let r = StaticRefresher {
            access: "new-access".into(),
            refresh: Some("new-refresh".into()),
            expires_at: new_expiry,
        };
        let updated = vault.refresh_if_expiring(id, now, None, &r).unwrap();
        assert_eq!(updated.access_token.expose(), "new-access");
        assert_eq!(
            updated.refresh_token.as_ref().map(SecretToken::expose),
            Some("new-refresh")
        );
        assert_eq!(updated.expires_at, new_expiry);
    }

    #[test]
    fn refresh_skipped_when_token_is_fresh() {
        let mut vault = OAuth2TokenVault::new();
        let id = ConnectorInstanceId::new_v4();
        let now = Utc::now();
        let tok = OAuth2Token::new(
            "fresh-access",
            "old-refresh",
            now + Duration::hours(2),
            "scope",
        );
        vault.put(id, tok);
        let r = StaticRefresher {
            access: "should-not-be-used".into(),
            refresh: None,
            expires_at: now + Duration::hours(3),
        };
        let unchanged = vault.refresh_if_expiring(id, now, None, &r).unwrap();
        assert_eq!(unchanged.access_token.expose(), "fresh-access");
    }

    #[test]
    fn refresh_skew_proactively_refreshes() {
        let mut vault = OAuth2TokenVault::new().with_default_skew(Duration::seconds(120));
        let id = ConnectorInstanceId::new_v4();
        let now = Utc::now();
        // Expires in 30s, well within the 120s skew window.
        let tok = OAuth2Token::new(
            "soon-to-expire",
            "old-refresh",
            now + Duration::seconds(30),
            "scope",
        );
        vault.put(id, tok);
        let r = StaticRefresher {
            access: "after-skew-refresh".into(),
            refresh: None,
            expires_at: now + Duration::hours(1),
        };
        let updated = vault.refresh_if_expiring(id, now, None, &r).unwrap();
        assert_eq!(updated.access_token.expose(), "after-skew-refresh");
        // Refresh token preserved when refresher omits one.
        assert_eq!(
            updated.refresh_token.as_ref().map(SecretToken::expose),
            Some("old-refresh")
        );
    }

    /// When a stored token has *no* refresh token (provider never
    /// issued one), `refresh_if_expiring` must short-circuit with
    /// `ConnectorError::TokenRefresh` rather than calling the
    /// refresher with an empty string and leaking the misleading
    /// provider error back to the caller.
    #[test]
    fn refresh_short_circuits_when_no_refresh_token_stored() {
        let mut vault = OAuth2TokenVault::new();
        let id = ConnectorInstanceId::new_v4();
        let now = Utc::now();
        // Expired token with NO refresh token — Slack legacy / PKCE
        // public-client style.
        vault.put(
            id,
            OAuth2Token::new_without_refresh("old-access", now - Duration::seconds(5), "scope"),
        );
        // The refresher must never run — use the failing one to
        // prove that even a working refresher would never get called.
        let err = vault
            .refresh_if_expiring(id, now, None, &FailingRefresher)
            .expect_err("must short-circuit with TokenRefresh");
        match err {
            ConnectorError::TokenRefresh(msg) => {
                assert!(msg.contains("no refresh_token stored"));
                assert!(
                    !msg.contains("provider rejected"),
                    "must not have called the refresher: {msg}"
                );
            }
            other => panic!("expected TokenRefresh, got {other:?}"),
        }
    }

    #[test]
    fn refresh_failure_propagates() {
        let mut vault = OAuth2TokenVault::new();
        let id = ConnectorInstanceId::new_v4();
        let now = Utc::now();
        vault.put(
            id,
            OAuth2Token::new("a", "r", now - Duration::seconds(1), "scope"),
        );
        let err = vault
            .refresh_if_expiring(id, now, None, &FailingRefresher)
            .unwrap_err();
        assert!(matches!(err, ConnectorError::TokenRefresh(_)));
    }

    #[test]
    fn debug_does_not_leak_tokens() {
        let s = format!(
            "{:?}",
            OAuth2Token::new("supersecret", "refresh", Utc::now(), "scope")
        );
        assert!(!s.contains("supersecret"));
        assert!(s.contains("redacted"));
    }
}
