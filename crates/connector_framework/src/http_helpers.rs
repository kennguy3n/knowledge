//! Shared HTTP helpers used by every connector.
//!
//! Each of the nine provider connectors performs the same handful
//! of low-level operations:
//!
//! * GET a JSON list / page endpoint with a bearer token, optionally
//!   carrying extra headers (e.g. `Notion-Version`).
//! * POST a JSON body to a provider endpoint with a bearer token
//!   (search APIs, webhook subscriptions, batch reads).
//! * Decide whether a non-2xx status maps to
//!   [`ConnectorError::Auth`] (401 / 403 — the provider rejected
//!   credentials, host must re-authenticate) or
//!   [`ConnectorError::Sync`] (everything else — transient or
//!   provider-bug, the runtime should retry the sync run).
//!
//! Centralising the boilerplate here keeps every connector's
//! `initial_sync` / `incremental_sync` / `subscribe_webhook` to
//! "build the URL, parse the response into the connector's serde
//! types" without each one repeating the bearer header / status
//! check / JSON parse / error-classify dance.
//!
//! The helpers stay sync (they call the blocking [`HttpTransport`])
//! to match the [`crate::Connector`] trait, which is invoked from
//! `spawn_blocking` on the async runtime side.

use std::sync::Arc;

use serde::de::DeserializeOwned;

use crate::error::{ConnectorError, Result};
use crate::http::{HttpRequest, HttpResponse, HttpTransport};
use crate::token_vault::OAuth2Token;

/// Map a non-2xx [`HttpResponse`] onto the appropriate
/// [`ConnectorError`] variant.
///
/// * `401` / `403` → [`ConnectorError::Auth`] so the runtime triggers
///   a re-authorisation prompt instead of indefinitely retrying with
///   an invalidated credential.
/// * Everything else → [`ConnectorError::Sync`] tagged with the
///   status code and (best-effort) body text. The substrate's
///   connector runtime treats `Sync` as retriable and reschedules
///   the sync run.
///
/// The body is read once via [`String::from_utf8_lossy`] so binary
/// or partially-binary responses don't surface raw bytes in the
/// error message — the lossy decode renders unprintable bytes as
/// the Unicode replacement character without panicking.
#[must_use]
pub fn classify_failure(provider: &str, endpoint: &str, resp: &HttpResponse) -> ConnectorError {
    let body = String::from_utf8_lossy(&resp.body);
    let trimmed = body.trim();
    let detail = if trimmed.is_empty() {
        "<empty body>".to_string()
    } else if trimmed.len() > 512 {
        // Cap so a paginated HTML error page from a misconfigured
        // gateway doesn't blow up a structured log line.
        format!("{}…", &trimmed[..512])
    } else {
        trimmed.to_string()
    };
    let msg = format!(
        "{provider} {endpoint} returned status {}: {detail}",
        resp.status
    );
    match resp.status {
        401 | 403 => ConnectorError::Auth(msg),
        _ => ConnectorError::Sync(msg),
    }
}

/// Issue a bearer-authenticated `GET` and parse the JSON response.
///
/// `extra_headers` is appended after `Authorization` / `Accept` so
/// providers that require additional version pins (Notion's
/// `Notion-Version: 2022-06-28`, GitHub's `X-GitHub-Api-Version`) or
/// custom tenancy markers can supply them. The transport handles
/// retries and `Retry-After` honouring.
///
/// # Errors
///
/// Returns [`ConnectorError::Transport`] for low-level network
/// failures, [`ConnectorError::Auth`] for 401/403, and
/// [`ConnectorError::Sync`] for any other non-2xx status or a JSON
/// parse failure.
pub fn bearer_get_json<R: DeserializeOwned>(
    transport: &Arc<dyn HttpTransport>,
    provider: &str,
    endpoint: &str,
    url: &str,
    token: &OAuth2Token,
    extra_headers: &[(&str, &str)],
) -> Result<R> {
    let mut req = HttpRequest::get(url)
        .with_bearer(token.access_token.expose())
        .with_header("Accept", "application/json");
    for (k, v) in extra_headers {
        req = req.with_header(*k, *v);
    }
    let resp = transport.execute(req)?;
    if !resp.is_success() {
        return Err(classify_failure(provider, endpoint, &resp));
    }
    serde_json::from_slice::<R>(&resp.body).map_err(|e| {
        ConnectorError::Sync(format!(
            "{provider} {endpoint} JSON parse failed: {e} \
             (body prefix: {})",
            String::from_utf8_lossy(&resp.body[..resp.body.len().min(256)])
        ))
    })
}

/// Issue a bearer-authenticated `POST` with a JSON body and parse
/// the JSON response.
///
/// The body is serialised via `serde_json::to_vec`; the
/// `Content-Type: application/json` header is set automatically.
/// `extra_headers` lets the caller supply provider-specific
/// version pins or tenancy markers.
///
/// # Errors
///
/// Returns [`ConnectorError::Transport`] for low-level network
/// failures, [`ConnectorError::Auth`] for 401/403, and
/// [`ConnectorError::Sync`] for non-2xx, JSON serialise, or JSON
/// parse failures.
pub fn bearer_post_json<B: serde::Serialize, R: DeserializeOwned>(
    transport: &Arc<dyn HttpTransport>,
    provider: &str,
    endpoint: &str,
    url: &str,
    token: &OAuth2Token,
    extra_headers: &[(&str, &str)],
    body: &B,
) -> Result<R> {
    let body_bytes = serde_json::to_vec(body).map_err(|e| {
        ConnectorError::Sync(format!(
            "{provider} {endpoint} serialise request body failed: {e}"
        ))
    })?;
    let mut req = HttpRequest::post(url, body_bytes)
        .with_bearer(token.access_token.expose())
        .with_header("Accept", "application/json")
        .with_header("Content-Type", "application/json");
    for (k, v) in extra_headers {
        req = req.with_header(*k, *v);
    }
    let resp = transport.execute(req)?;
    if !resp.is_success() {
        return Err(classify_failure(provider, endpoint, &resp));
    }
    serde_json::from_slice::<R>(&resp.body).map_err(|e| {
        ConnectorError::Sync(format!(
            "{provider} {endpoint} JSON parse failed: {e} \
             (body prefix: {})",
            String::from_utf8_lossy(&resp.body[..resp.body.len().min(256)])
        ))
    })
}

/// Issue a bearer-authenticated `POST` with a form-encoded body and
/// parse the JSON response.
///
/// Some providers (Slack `oauth.v2.access`, Atlassian's older
/// `oauth/token` mode, Figma's webhook subscribe) accept
/// `application/x-www-form-urlencoded` instead of JSON.
///
/// # Errors
///
/// Same as [`bearer_post_json`].
pub fn bearer_post_form<R: DeserializeOwned>(
    transport: &Arc<dyn HttpTransport>,
    provider: &str,
    endpoint: &str,
    url: &str,
    token: &OAuth2Token,
    extra_headers: &[(&str, &str)],
    form: &[(&str, &str)],
) -> Result<R> {
    let body = encode_form(form);
    let mut req = HttpRequest::post(url, body.into_bytes())
        .with_bearer(token.access_token.expose())
        .with_header("Accept", "application/json")
        .with_header("Content-Type", "application/x-www-form-urlencoded");
    for (k, v) in extra_headers {
        req = req.with_header(*k, *v);
    }
    let resp = transport.execute(req)?;
    if !resp.is_success() {
        return Err(classify_failure(provider, endpoint, &resp));
    }
    serde_json::from_slice::<R>(&resp.body).map_err(|e| {
        ConnectorError::Sync(format!(
            "{provider} {endpoint} JSON parse failed: {e} \
             (body prefix: {})",
            String::from_utf8_lossy(&resp.body[..resp.body.len().min(256)])
        ))
    })
}

/// Percent-encode a single value per RFC 3986 §2 — the subset used
/// by `application/x-www-form-urlencoded` bodies.
///
/// We deliberately do **not** pull in a dedicated `url` /
/// `percent-encoding` dependency for this — the only consumers are
/// `encode_form` (for OAuth2 token endpoint bodies) and the
/// per-provider `incremental_sync` URL builders, and the algorithm
/// is small enough to inline. Mirrors the implementation in
/// `crate::oauth` so the framework speaks one dialect.
#[must_use]
pub fn percent_encode_form_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            b' ' => out.push('+'),
            _ => {
                out.push('%');
                out.push(hex_nibble(b >> 4));
                out.push(hex_nibble(b & 0x0F));
            }
        }
    }
    out
}

fn hex_nibble(n: u8) -> char {
    match n {
        0..=9 => (b'0' + n) as char,
        10..=15 => (b'A' + (n - 10)) as char,
        _ => unreachable!("nibble in 0..=15 by construction"),
    }
}

/// Encode a `(name, value)` slice as an `application/x-www-form-urlencoded`
/// body. The `&` separator is between pairs, `=` between name and
/// value. Both sides are percent-encoded.
#[must_use]
pub fn encode_form(form: &[(&str, &str)]) -> String {
    let mut out = String::new();
    for (i, (k, v)) in form.iter().enumerate() {
        if i > 0 {
            out.push('&');
        }
        out.push_str(&percent_encode_form_component(k));
        out.push('=');
        out.push_str(&percent_encode_form_component(v));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::HttpResponse;

    #[test]
    fn classify_failure_401_maps_to_auth() {
        let resp = HttpResponse {
            status: 401,
            headers: vec![],
            body: br#"{"error":"invalid_token"}"#.to_vec(),
        };
        let err = classify_failure("slack", "conversations.list", &resp);
        assert!(matches!(err, ConnectorError::Auth(_)), "got {err:?}");
    }

    #[test]
    fn classify_failure_403_maps_to_auth() {
        let resp = HttpResponse {
            status: 403,
            headers: vec![],
            body: br#"{"error":"missing_scope"}"#.to_vec(),
        };
        let err = classify_failure("slack", "conversations.list", &resp);
        assert!(matches!(err, ConnectorError::Auth(_)), "got {err:?}");
    }

    #[test]
    fn classify_failure_500_maps_to_sync() {
        let resp = HttpResponse {
            status: 500,
            headers: vec![],
            body: br#"{"error":"internal"}"#.to_vec(),
        };
        let err = classify_failure("slack", "conversations.list", &resp);
        assert!(matches!(err, ConnectorError::Sync(_)), "got {err:?}");
    }

    #[test]
    fn classify_failure_empty_body_renders_placeholder() {
        let resp = HttpResponse {
            status: 502,
            headers: vec![],
            body: vec![],
        };
        let err = classify_failure("notion", "search", &resp);
        let msg = format!("{err}");
        assert!(msg.contains("<empty body>"), "got {msg}");
    }

    #[test]
    fn classify_failure_long_body_truncates() {
        let body = "a".repeat(2_000).into_bytes();
        let resp = HttpResponse {
            status: 500,
            headers: vec![],
            body,
        };
        let err = classify_failure("hubspot", "contacts.list", &resp);
        let msg = format!("{err}");
        assert!(msg.contains('…'), "expected ellipsis: {msg}");
        // Truncated body cap is 512 + ellipsis; full 2000 bytes
        // must not appear verbatim.
        assert!(
            !msg.contains(&"a".repeat(1_000)),
            "long body must be truncated"
        );
    }

    #[test]
    fn percent_encode_spaces_use_plus() {
        assert_eq!(percent_encode_form_component("hello world"), "hello+world");
    }

    #[test]
    fn percent_encode_special_chars() {
        // `&` and `=` must be percent-encoded so they don't get
        // mistaken for form delimiters.
        assert_eq!(percent_encode_form_component("a&b=c"), "a%26b%3Dc");
    }

    #[test]
    fn percent_encode_preserves_unreserved() {
        // RFC 3986 §2.3 unreserved set: a-z A-Z 0-9 -._~
        assert_eq!(
            percent_encode_form_component("Az0-_.~"),
            "Az0-_.~",
            "unreserved characters must pass through unchanged"
        );
    }

    #[test]
    fn encode_form_concatenates_pairs() {
        let body = encode_form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", "abc def"),
        ]);
        assert_eq!(body, "grant_type=refresh_token&refresh_token=abc+def");
    }
}
