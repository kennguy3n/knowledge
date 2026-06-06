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
        // gateway doesn't blow up a structured log line. Walk
        // back from byte 512 to the previous UTF-8 char boundary
        // — `from_utf8_lossy` may have substituted invalid bytes
        // with U+FFFD (3 bytes each), so a naive `&trimmed[..512]`
        // would panic when the cap falls inside a multi-byte
        // sequence. `str::is_char_boundary(0)` is always true, so
        // the loop is guaranteed to terminate.
        let mut end = 512;
        while !trimmed.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &trimmed[..end])
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

/// Attach the request auth header that matches a token's *provenance*.
///
/// Some providers authenticate a static credential (an API key,
/// access token, or session token supplied in the connector config)
/// through a provider-native header — e.g. `X-Api-Key`,
/// `X-Sapo-Access-Token`, `tiki-api-key`, or `X-Gojek-Api-Key` — but
/// also support an OAuth2 code-exchange fallback whose token is a
/// standard bearer credential. Sending an OAuth-issued token in the
/// provider-native header (or, conversely, a static key as
/// `Authorization: Bearer`) is wrong, so the connector records the
/// credential's origin in [`OAuth2Token::token_type`] at
/// `authenticate` time and dispatches here:
///
/// * `token.token_type == marker` → the credential is the static one,
///   so it goes in `native_header`.
/// * otherwise → the credential came from OAuth, so it is sent as
///   `Authorization: <scheme> <token>`, where `scheme` is the token's
///   `token_type` (defaulting to `Bearer` when empty).
///
/// `marker` is the connector-private sentinel string the connector
/// also assigns to `token_type` for its static path (e.g. `"ApiKey"`,
/// `"AccessToken"`, `"Session"`); it is only ever compared against the
/// same connector's own tokens, so its exact spelling is arbitrary.
///
/// This deliberately does not touch any other headers — request
/// signing (HMAC `sign`/`X-Signature` pairs keyed by a separate
/// merchant secret) is layered on independently by the caller.
#[must_use]
pub fn apply_auth_by_provenance(
    req: HttpRequest,
    token: &OAuth2Token,
    native_header: &str,
    marker: &str,
) -> HttpRequest {
    if token.token_type == marker {
        req.with_header(native_header, token.access_token.expose())
    } else {
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
}

/// Percent-encode a single value per RFC 3986 §2 — the subset used
/// by `application/x-www-form-urlencoded` bodies.
///
/// This encoder is **only** for the body of an
/// `application/x-www-form-urlencoded` POST: it encodes a literal
/// space as `+`, matching the WHATWG URL-living-standard /
/// HTML5 form-submission algorithm. For URL **query strings** and
/// path segments, use [`percent_encode_path_component`] which
/// emits `%20` per RFC 3986 §3.4.
///
/// We deliberately do **not** pull in a dedicated `url` /
/// `percent-encoding` dependency — the only consumers are
/// `encode_form` (for OAuth2 token endpoint bodies) and the
/// per-provider `incremental_sync` URL builders, and the algorithm
/// is small enough to inline. Mirrors the implementation in
/// `crate::oauth` so the framework speaks one dialect.
#[must_use]
pub fn percent_encode_form_component(s: &str) -> String {
    encode_with_space(s, true)
}

/// Percent-encode a single value as a URL path / query component
/// per RFC 3986 §2.3 / §3.4. Spaces are emitted as `%20`, not `+`,
/// so the result is correct for `?key=value` query parameters and
/// `/{segment}` path components — including the case where a strict
/// RFC 3986 proxy or gateway sits between the substrate and the
/// provider and would reject `+` in a query string.
///
/// Use this (not [`percent_encode_form_component`]) for any string
/// that becomes part of the URL, including cursor / pagination
/// tokens, search queries, etc.
#[must_use]
pub fn percent_encode_path_component(s: &str) -> String {
    encode_with_space(s, false)
}

#[inline]
fn encode_with_space(s: &str, space_as_plus: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            b' ' if space_as_plus => out.push('+'),
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
    fn classify_failure_truncation_respects_utf8_boundary() {
        // Pad with 510 ASCII bytes, then two invalid UTF-8 bytes
        // so `from_utf8_lossy` substitutes them with two U+FFFD
        // characters (3 bytes each). The resulting `&str` is
        // 510 + 6 = 516 bytes long, so the trim/cap branch fires
        // and tries to slice at byte 512 — that falls *inside*
        // the first U+FFFD (bytes 510..513), which would panic
        // on a naive `&str[..512]`. The safe truncation walks
        // back to byte 510 (the prior char boundary) and renders
        // the prefix without crashing.
        let mut body = b"a".repeat(510);
        body.extend_from_slice(&[0xFF, 0xFF]);
        let resp = HttpResponse {
            status: 502,
            headers: vec![],
            body,
        };
        let err = classify_failure("notion", "/v1/search", &resp);
        let msg = format!("{err}");
        assert!(msg.contains('…'), "expected truncation ellipsis: {msg}");
        // The 510 ASCII bytes must be preserved verbatim.
        assert!(
            msg.contains(&"a".repeat(510)),
            "expected the 510-byte ASCII prefix in the error message"
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
    fn percent_encode_path_component_uses_pct20_for_spaces() {
        // A strict RFC 3986 §3.4 proxy / gateway may reject `+` in a
        // query string; the URL encoder must emit `%20` so the
        // substrate's traffic is never rejected mid-path.
        assert_eq!(
            percent_encode_path_component("hello world"),
            "hello%20world"
        );
    }

    #[test]
    fn percent_encode_path_component_round_trips_jql_with_spaces() {
        // Regression: Jira's JQL is the most common spaces-in-URL
        // case in this PR (`updated > 2024-01-01 ORDER BY updated`).
        // Asserting `%20` here pins the encoder choice in the URL
        // path against accidental migration back to `+`.
        let encoded = percent_encode_path_component("updated > '2024-01-01' ORDER BY updated ASC");
        assert!(
            encoded.contains("%20"),
            "expected spaces as %20, got: {encoded}"
        );
        assert!(
            !encoded.contains('+'),
            "URL encoder must not emit `+` for spaces; got: {encoded}"
        );
    }

    #[test]
    fn percent_encode_path_and_form_agree_on_non_space_special_chars() {
        // The two encoders only differ on the space character —
        // every other reserved byte must be percent-encoded the
        // same way. This guards against an accidental divergence
        // (e.g. exempting `&` from one but not the other) that
        // would silently break URL parsing.
        let payload = "a&b=c/d?e#f g";
        let form = percent_encode_form_component(payload);
        let path = percent_encode_path_component(payload);
        // The form encoder writes `+` for ' '; the path encoder
        // writes `%20`. Other special bytes must match.
        assert_eq!(form.replace('+', "%20"), path);
    }

    #[test]
    fn encode_form_concatenates_pairs() {
        let body = encode_form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", "abc def"),
        ]);
        assert_eq!(body, "grant_type=refresh_token&refresh_token=abc+def");
    }

    fn token_with_type(access: &str, token_type: &str) -> OAuth2Token {
        let mut t = OAuth2Token::new_without_refresh(
            access,
            chrono::Utc::now() + chrono::Duration::hours(1),
            "scope",
        );
        t.token_type = token_type.to_string();
        t
    }

    fn header<'a>(req: &'a HttpRequest, name: &str) -> Option<&'a str> {
        req.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn provenance_static_marker_uses_native_header() {
        let token = token_with_type("static-key", "ApiKey");
        let req = apply_auth_by_provenance(
            HttpRequest::get("https://api.test/x"),
            &token,
            "X-Api-Key",
            "ApiKey",
        );
        assert_eq!(header(&req, "X-Api-Key"), Some("static-key"));
        assert_eq!(header(&req, "Authorization"), None);
    }

    #[test]
    fn provenance_oauth_token_uses_bearer() {
        // OAuth-issued token keeps its provider type ("Bearer") and so
        // does not match the static marker.
        let token = token_with_type("oauth-tok", "Bearer");
        let req = apply_auth_by_provenance(
            HttpRequest::get("https://api.test/x"),
            &token,
            "X-Api-Key",
            "ApiKey",
        );
        assert_eq!(header(&req, "Authorization"), Some("Bearer oauth-tok"));
        assert_eq!(header(&req, "X-Api-Key"), None);
    }

    #[test]
    fn provenance_empty_token_type_defaults_to_bearer() {
        let token = token_with_type("tok", "");
        let req = apply_auth_by_provenance(
            HttpRequest::get("https://api.test/x"),
            &token,
            "X-Api-Key",
            "ApiKey",
        );
        assert_eq!(header(&req, "Authorization"), Some("Bearer tok"));
    }

    #[test]
    fn provenance_preserves_non_marker_scheme() {
        // A non-empty, non-marker token_type is used verbatim as the
        // Authorization scheme.
        let token = token_with_type("tok", "DPoP");
        let req = apply_auth_by_provenance(
            HttpRequest::get("https://api.test/x"),
            &token,
            "X-Api-Key",
            "ApiKey",
        );
        assert_eq!(header(&req, "Authorization"), Some("DPoP tok"));
    }
}
