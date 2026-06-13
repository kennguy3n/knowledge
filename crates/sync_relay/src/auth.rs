//! Bearer-token authentication and per-tenant isolation.
//!
//! The relay is *authenticated* but still *untrusted*: a bearer
//! token gates **who** may push/pull, and the token determines the
//! [`TenantId`] under which a topic's blobs are namespaced — but the
//! relay still only ever sees opaque ciphertext. Authentication and
//! confidentiality are orthogonal here: the token controls access to
//! the store-and-forward buffer, while the per-scope AEAD seal key
//! (which the relay never holds) controls who can read the contents.
//!
//! Tenant namespacing is what keeps 5 000 SME tenants isolated on one
//! relay: tenant A's token can never read tenant B's topics even if A
//! somehow learns B's [`TopicId`], because the storage key is
//! `(tenant, topic)`, not `topic` alone.

use std::collections::HashMap;

/// Opaque tenant identifier. One SME tenant ↔ one (or more) bearer
/// tokens; all of a tenant's devices share the tenant's token(s) and
/// therefore the same `(tenant, topic)` storage namespace.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TenantId(String);

impl TenantId {
    /// Wrap a tenant identifier string.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Borrow the identifier string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for TenantId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Maps bearer tokens to the tenant they authenticate.
///
/// Tokens are high-entropy secrets; lookup is by exact match. The
/// registry is built once at startup and shared (read-only) across
/// all requests, so a plain map suffices — there is no per-request
/// allocation or lock contention beyond the `Arc` the server wraps it
/// in.
#[derive(Debug, Default)]
pub struct TokenRegistry {
    tokens: HashMap<String, TenantId>,
}

impl TokenRegistry {
    /// Build an empty registry. A relay with no tokens rejects every
    /// request — fail closed.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `token` as authenticating `tenant`. Re-registering a
    /// token rebinds it (last write wins).
    pub fn insert(&mut self, token: impl Into<String>, tenant: TenantId) {
        self.tokens.insert(token.into(), tenant);
    }

    /// Build a registry from `token:tenant` pairs (e.g. parsed from a
    /// `RELAY_TOKENS` env var). Whitespace around entries is trimmed;
    /// blank entries are skipped. Returns `None` if any non-blank
    /// entry is missing its `:` separator or has an empty token /
    /// tenant — fail closed rather than silently dropping a
    /// misconfigured credential.
    pub fn from_pairs(spec: &str) -> Option<Self> {
        let mut registry = Self::new();
        for entry in spec.split(',') {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            let (token, tenant) = entry.split_once(':')?;
            let (token, tenant) = (token.trim(), tenant.trim());
            if token.is_empty() || tenant.is_empty() {
                return None;
            }
            registry.insert(token, TenantId::new(tenant));
        }
        Some(registry)
    }

    /// Number of registered tokens.
    pub fn len(&self) -> usize {
        self.tokens.len()
    }

    /// Whether the registry has no tokens (and so rejects everything).
    pub fn is_empty(&self) -> bool {
        self.tokens.is_empty()
    }

    /// Resolve a presented bearer token to its tenant, or `None` if
    /// the token is unknown.
    pub fn authenticate(&self, token: &str) -> Option<&TenantId> {
        self.tokens.get(token)
    }
}

/// Extract the bearer token from an `Authorization: Bearer <token>`
/// header value. Returns `None` for any other scheme or a malformed
/// header.
pub fn bearer_token(header_value: &str) -> Option<&str> {
    let token = header_value.strip_prefix("Bearer ")?;
    let token = token.trim();
    if token.is_empty() {
        None
    } else {
        Some(token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_pairs_parses_and_rejects() {
        let reg = TokenRegistry::from_pairs("tok-a:tenant-1, tok-b:tenant-2").unwrap();
        assert_eq!(reg.len(), 2);
        assert_eq!(reg.authenticate("tok-a").unwrap().as_str(), "tenant-1");
        assert_eq!(reg.authenticate("tok-b").unwrap().as_str(), "tenant-2");
        assert!(reg.authenticate("nope").is_none());

        // Blank entries are skipped.
        assert_eq!(TokenRegistry::from_pairs("tok:ten,,").unwrap().len(), 1);
        // Missing separator / empty halves fail closed.
        assert!(TokenRegistry::from_pairs("no-colon").is_none());
        assert!(TokenRegistry::from_pairs(":tenant").is_none());
        assert!(TokenRegistry::from_pairs("token:").is_none());
    }

    #[test]
    fn bearer_token_parsing() {
        assert_eq!(bearer_token("Bearer abc123"), Some("abc123"));
        assert_eq!(bearer_token("Bearer   spaced  "), Some("spaced"));
        assert_eq!(bearer_token("bearer abc"), None); // case-sensitive scheme
        assert_eq!(bearer_token("Basic abc"), None);
        assert_eq!(bearer_token("Bearer "), None);
        assert_eq!(bearer_token(""), None);
    }
}
