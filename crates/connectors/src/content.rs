//! Shared helpers for the connectors' `fetch_content` implementations.
//!
//! Every connector's [`fetch_content`](connector_framework::Connector::fetch_content)
//! lands on the same handful of low-level operations that the JSON
//! helpers in `connector_framework::http_helpers` don't cover:
//!
//! * GET an endpoint with a bearer token and keep the **raw** response
//!   bytes (binary downloads: Drive `alt=media`, OneDrive
//!   `/content`, Slack `url_private_download`) instead of parsing JSON.
//! * Decode `base64` / `base64url` payloads (Gmail delivers MIME parts
//!   base64url-encoded).
//! * Flatten provider rich-text trees — Atlassian ADF JSON and
//!   Confluence / email XHTML — into plain UTF-8 text.
//!
//! These live in the `connectors` crate (not `connector_framework`)
//! because they are connector-implementation detail, and the crate
//! deliberately avoids adding a `base64` / `html` dependency for what
//! amounts to a few hundred bytes of straightforward parsing.

use std::sync::Arc;

use connector_framework::{
    classify_failure, HttpRequest, HttpResponse, HttpTransport, OAuth2Token, Result,
};

/// Issue a bearer-authenticated `GET` and return the raw
/// [`HttpResponse`] (body bytes + headers) after a success check.
///
/// This is the binary-download analogue of
/// `connector_framework::bearer_get_json`: it does **not** parse the
/// body, so callers that need the literal file bytes (Drive
/// `alt=media`, OneDrive `/content`, Slack file downloads) or want to
/// inspect the `Content-Type` header keep full control.
///
/// `extra_headers` is appended after `Authorization`, so callers can
/// add a `Range` header (Drive partial download) or a provider version
/// pin. Non-2xx responses are mapped through
/// [`classify_failure`](connector_framework::classify_failure) — 401/403
/// → `Auth`, everything else → `Sync` — to match the JSON helpers.
///
/// # Errors
///
/// Returns [`ConnectorError::Transport`](connector_framework::ConnectorError::Transport)
/// for low-level network failures and the classified error for any
/// non-2xx status.
pub(crate) fn bearer_get_raw(
    transport: &Arc<dyn HttpTransport>,
    provider: &str,
    endpoint: &str,
    url: &str,
    token: &OAuth2Token,
    extra_headers: &[(&str, &str)],
) -> Result<HttpResponse> {
    let mut req = HttpRequest::get(url).with_bearer(token.access_token.expose());
    for (k, v) in extra_headers {
        req = req.with_header(*k, *v);
    }
    let resp = transport.execute(req)?;
    if !resp.is_success() {
        return Err(classify_failure(provider, endpoint, &resp));
    }
    Ok(resp)
}

/// Issue a plain (unauthenticated) `GET` and return the raw
/// [`HttpResponse`] after a success check.
///
/// Used for provider-issued **pre-signed** download URLs — Microsoft
/// Graph's `@microsoft.graph.downloadUrl` embeds its own short-lived
/// credential in the query string, and attaching the connector's
/// bearer token on top can trip the CDN / SharePoint host that serves
/// the bytes. (Slack file downloads are the opposite — they *require*
/// the bot-token bearer — so those go through [`bearer_get_raw`].)
/// `extra_headers` still lets callers add a `Range` or
/// provider-specific header.
///
/// # Errors
///
/// Returns [`ConnectorError::Transport`](connector_framework::ConnectorError::Transport)
/// for low-level network failures and the classified error for any
/// non-2xx status.
pub(crate) fn get_raw(
    transport: &Arc<dyn HttpTransport>,
    provider: &str,
    endpoint: &str,
    url: &str,
    extra_headers: &[(&str, &str)],
) -> Result<HttpResponse> {
    let mut req = HttpRequest::get(url);
    for (k, v) in extra_headers {
        req = req.with_header(*k, *v);
    }
    let resp = transport.execute(req)?;
    if !resp.is_success() {
        return Err(classify_failure(provider, endpoint, &resp));
    }
    Ok(resp)
}

/// Look up a response header value by (case-insensitive) name.
///
/// [`HttpResponse::headers`] are stored lower-cased on the name side by
/// the transport, so we lower-case the needle once and compare.
#[must_use]
pub(crate) fn response_header<'a>(resp: &'a HttpResponse, name: &str) -> Option<&'a str> {
    let needle = name.to_ascii_lowercase();
    resp.headers
        .iter()
        .find(|(k, _)| k == &needle)
        .map(|(_, v)| v.as_str())
}

/// Strip any `; charset=…` / `; boundary=…` parameters from a
/// `Content-Type` header, returning the bare media type (trimmed,
/// lower-cased on the way out is *not* done — providers already send a
/// canonical type and downstream comparisons are case-insensitive).
///
/// `text/plain; charset=UTF-8` → `text/plain`.
#[must_use]
pub(crate) fn strip_charset(content_type: &str) -> &str {
    content_type
        .split(';')
        .next()
        .map_or(content_type, str::trim)
}

/// Decode a `base64` / `base64url` string, tolerating both alphabets,
/// missing padding, and embedded ASCII whitespace (newlines in MIME
/// bodies, `\r\n` line wrapping).
///
/// Gmail returns message-part bodies as base64url **without** padding;
/// some MIME stacks wrap standard base64 at 76 columns. A single
/// tolerant decoder handles every shape the email connector sees.
///
/// # Errors
///
/// Returns `None` if the input contains a character outside both
/// alphabets (after whitespace is stripped) or has an invalid trailing
/// quantum.
#[must_use]
pub(crate) fn decode_base64(input: &str) -> Option<Vec<u8>> {
    fn val(b: u8) -> Option<u8> {
        match b {
            b'A'..=b'Z' => Some(b - b'A'),
            b'a'..=b'z' => Some(b - b'a' + 26),
            b'0'..=b'9' => Some(b - b'0' + 52),
            // Accept both standard (`+`, `/`) and url-safe (`-`, `_`).
            b'+' | b'-' => Some(62),
            b'/' | b'_' => Some(63),
            _ => None,
        }
    }

    let mut quad = [0u8; 4];
    let mut n = 0usize;
    let mut out = Vec::with_capacity(input.len() / 4 * 3 + 3);
    for &b in input.as_bytes() {
        match b {
            b'=' => break,
            b if b.is_ascii_whitespace() => continue,
            b => {
                quad[n] = val(b)?;
                n += 1;
                if n == 4 {
                    out.push((quad[0] << 2) | (quad[1] >> 4));
                    out.push((quad[1] << 4) | (quad[2] >> 2));
                    out.push((quad[2] << 6) | quad[3]);
                    n = 0;
                }
            }
        }
    }
    match n {
        0 => {}
        1 => return None, // a lone sextet can't encode any byte
        2 => out.push((quad[0] << 2) | (quad[1] >> 4)),
        3 => {
            out.push((quad[0] << 2) | (quad[1] >> 4));
            out.push((quad[1] << 4) | (quad[2] >> 2));
        }
        _ => unreachable!("n < 4 by construction"),
    }
    Some(out)
}

/// Flatten an HTML / XHTML fragment into plain UTF-8 text.
///
/// Used for Confluence `storage`-format bodies and `text/html` email
/// parts. This is deliberately a *stripper*, not a renderer: it drops
/// every tag, turns block-level boundaries (`</p>`, `<br>`, `</div>`,
/// `</li>`, `</h1>`…`</h6>`, `</tr>`) into newlines, decodes the common
/// named / numeric entities, and collapses runs of intra-line
/// whitespace. The result is suitable for embedding / full-text
/// indexing, which is all the substrate needs from a stored page.
#[must_use]
pub(crate) fn strip_html(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let bytes = html.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'<' => {
                // Find the matching `>`; if unterminated, treat the
                // rest as text so we never lose content.
                let Some(rel) = html[i..].find('>') else {
                    out.push_str(&html[i..]);
                    break;
                };
                let tag = &html[i + 1..i + rel];
                if is_block_boundary_tag(tag) {
                    out.push('\n');
                }
                i += rel + 1;
            }
            b'&' => {
                // Decode an entity; on a malformed one, emit the
                // literal `&` and continue.
                if let Some((decoded, consumed)) = decode_entity(&html[i..]) {
                    out.push_str(&decoded);
                    i += consumed;
                } else {
                    out.push('&');
                    i += 1;
                }
            }
            _ => {
                // Copy one UTF-8 scalar verbatim.
                let ch = html[i..].chars().next().unwrap_or('\u{FFFD}');
                out.push(ch);
                i += ch.len_utf8();
            }
        }
    }
    collapse_whitespace(&out)
}

/// True for tags whose presence marks a line / block boundary, so the
/// stripped text keeps paragraph and list structure as newlines.
fn is_block_boundary_tag(tag: &str) -> bool {
    // Strip a leading `/` (closing tag) and any attributes after the
    // first whitespace, then lower-case the bare element name.
    let name = tag.trim_start_matches('/');
    let name = name.split([' ', '\t', '\n', '/']).next().unwrap_or("");
    let name = name.to_ascii_lowercase();
    matches!(
        name.as_str(),
        "p" | "br"
            | "div"
            | "li"
            | "tr"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "blockquote"
            | "pre"
            | "ul"
            | "ol"
            | "table"
            | "section"
            | "article"
    )
}

/// Decode one HTML entity at the start of `s` (which begins with `&`).
/// Returns the decoded string and the number of input bytes consumed.
fn decode_entity(s: &str) -> Option<(String, usize)> {
    let end = s[1..].find(';')? + 1;
    let body = &s[1..end];
    let decoded = match body {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" | "#39" => '\'',
        "nbsp" => ' ',
        _ if body.starts_with("#x") || body.starts_with("#X") => {
            let cp = u32::from_str_radix(&body[2..], 16).ok()?;
            char::from_u32(cp)?
        }
        _ if body.starts_with('#') => {
            let cp = body[1..].parse::<u32>().ok()?;
            char::from_u32(cp)?
        }
        _ => return None,
    };
    Some((decoded.to_string(), end + 1))
}

/// Collapse runs of spaces / tabs into a single space and trim trailing
/// spaces on each line, dropping blank lines so consecutive block
/// boundaries collapse to a single newline separator.
fn collapse_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for raw_line in s.split('\n') {
        let mut line = String::with_capacity(raw_line.len());
        let mut prev_space = false;
        for ch in raw_line.chars() {
            if ch.is_whitespace() {
                if !prev_space {
                    line.push(' ');
                    prev_space = true;
                }
            } else {
                line.push(ch);
                prev_space = false;
            }
        }
        let trimmed = line.trim();
        // Drop blank lines entirely. Adjacent block-boundary tags
        // (e.g. `</p><p>`) each emit a newline; skipping the resulting
        // empty lines keeps consecutive blocks separated by exactly
        // one `\n` in the extracted plain text.
        if !trimmed.is_empty() {
            out.push_str(trimmed);
            out.push('\n');
        }
    }
    out.trim().to_string()
}

/// Recursively flatten an Atlassian Document Format (ADF) node into
/// plain text.
///
/// ADF (used by Jira issue descriptions / comments) is a nested JSON
/// tree of typed nodes; `text` nodes carry a `text` field, container
/// nodes carry a `content` array, and `hardBreak` / `paragraph` mark
/// line boundaries. We walk the tree depth-first, emit each `text`
/// node's content, and insert newlines on block boundaries — enough to
/// recover a readable plain-text rendering for indexing.
#[must_use]
pub(crate) fn adf_to_text(node: &serde_json::Value) -> String {
    let mut out = String::new();
    walk_adf(node, 0, &mut out);
    collapse_whitespace(&out)
}

/// Recursion ceiling for [`walk_adf`]. Real ADF documents nest only a
/// handful of levels deep; this cap stops a pathological or maliciously
/// crafted payload from overflowing the stack.
const MAX_ADF_DEPTH: usize = 64;

fn walk_adf(node: &serde_json::Value, depth: usize, out: &mut String) {
    if depth >= MAX_ADF_DEPTH {
        return;
    }
    let node_type = node.get("type").and_then(serde_json::Value::as_str);
    match node_type {
        Some("text") => {
            if let Some(t) = node.get("text").and_then(serde_json::Value::as_str) {
                out.push_str(t);
            }
        }
        Some("hardBreak") => out.push('\n'),
        Some("mention") => {
            if let Some(t) = node
                .get("attrs")
                .and_then(|a| a.get("text"))
                .and_then(serde_json::Value::as_str)
            {
                out.push_str(t);
            }
        }
        _ => {}
    }
    if let Some(content) = node.get("content").and_then(serde_json::Value::as_array) {
        for child in content {
            walk_adf(child, depth + 1, out);
        }
    }
    // Block-level nodes terminate a line so adjacent paragraphs /
    // list items don't run together.
    if matches!(
        node_type,
        Some(
            "paragraph"
                | "heading"
                | "listItem"
                | "blockquote"
                | "codeBlock"
                | "tableRow"
                | "bulletList"
                | "orderedList"
        )
    ) {
        out.push('\n');
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_base64_standard_with_padding() {
        assert_eq!(decode_base64("aGVsbG8=").unwrap(), b"hello");
    }

    #[test]
    fn decode_base64_urlsafe_without_padding() {
        // "subjects?_d" style url-safe payload round-trips.
        let encoded = "aGVsbG8gd29ybGQ"; // "hello world", no padding
        assert_eq!(decode_base64(encoded).unwrap(), b"hello world");
    }

    #[test]
    fn decode_base64_ignores_whitespace() {
        let wrapped = "aGVs\r\nbG8=";
        assert_eq!(decode_base64(wrapped).unwrap(), b"hello");
    }

    #[test]
    fn decode_base64_urlsafe_special_chars() {
        // 0xFB 0xFF 0xBF encodes to "-_-_" in url-safe, "+/+/" standard.
        assert_eq!(decode_base64("-_-_").unwrap(), vec![0xFB, 0xFF, 0xBF]);
        assert_eq!(decode_base64("+/+/").unwrap(), vec![0xFB, 0xFF, 0xBF]);
    }

    #[test]
    fn decode_base64_rejects_invalid() {
        assert!(decode_base64("@@@@").is_none());
    }

    #[test]
    fn strip_html_drops_tags_and_keeps_text() {
        let html = "<p>Hello <strong>world</strong></p><p>Second line</p>";
        assert_eq!(strip_html(html), "Hello world\nSecond line");
    }

    #[test]
    fn strip_html_decodes_entities() {
        let html = "<p>a &amp; b &lt; c &gt; d &#39;e&#39; &#x41;</p>";
        assert_eq!(strip_html(html), "a & b < c > d 'e' A");
    }

    #[test]
    fn strip_html_handles_unterminated_tag() {
        // A `<` with no closing `>` must not panic or drop content.
        let html = "text <not closed";
        assert_eq!(strip_html(html), "text <not closed");
    }

    #[test]
    fn strip_html_br_becomes_newline() {
        assert_eq!(strip_html("line1<br/>line2"), "line1\nline2");
    }

    #[test]
    fn adf_to_text_flattens_paragraphs() {
        let adf = serde_json::json!({
            "type": "doc",
            "content": [
                { "type": "paragraph", "content": [
                    { "type": "text", "text": "First paragraph." }
                ]},
                { "type": "paragraph", "content": [
                    { "type": "text", "text": "Second " },
                    { "type": "text", "text": "paragraph." }
                ]}
            ]
        });
        assert_eq!(adf_to_text(&adf), "First paragraph.\nSecond paragraph.");
    }

    #[test]
    fn adf_to_text_handles_hard_break_and_mention() {
        let adf = serde_json::json!({
            "type": "paragraph",
            "content": [
                { "type": "text", "text": "Hi " },
                { "type": "mention", "attrs": { "text": "@alice" } },
                { "type": "hardBreak" },
                { "type": "text", "text": "bye" }
            ]
        });
        assert_eq!(adf_to_text(&adf), "Hi @alice\nbye");
    }

    #[test]
    fn response_header_is_case_insensitive() {
        let resp = HttpResponse {
            status: 200,
            headers: vec![("content-type".to_string(), "application/pdf".to_string())],
            body: vec![],
        };
        assert_eq!(
            response_header(&resp, "Content-Type"),
            Some("application/pdf")
        );
        assert_eq!(response_header(&resp, "missing"), None);
    }
}
