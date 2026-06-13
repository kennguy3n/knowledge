//! Cassette / VCR-style HTTP fixtures for deterministic connector
//! integration tests.
//!
//! Every connector talks to its source system over the
//! [`HttpTransport`] trait. The unit tests in each connector crate
//! drive that boundary with [`crate::http::MockHttpTransport`], which
//! is great for pinning *one* request/response pair inline but does
//! not capture a full multi-call lifecycle (OAuth2 refresh → full
//! sync → incremental sync → content fetch) as a single reviewable
//! artifact.
//!
//! A **cassette** is exactly that artifact: an ordered list of
//! `(request, response)` [interactions](HttpInteraction) serialised
//! to JSON on disk. Two transports operate on it:
//!
//! * [`ReplayTransport`] — loads a cassette and serves the recorded
//!   responses back, matching each live request against the recorded
//!   ones by `(method, url)`. It never touches the network, so a test
//!   built on it is fully deterministic and runs in ordinary
//!   (offline) CI. A request with no matching recorded interaction is
//!   a **hard error** ([`ConnectorError::Transport`]) rather than a
//!   silent 404 — an un-recorded call means the fixture is stale and
//!   the test should fail loudly.
//! * [`RecordingTransport`] — wraps any real [`HttpTransport`] (e.g.
//!   the reqwest-backed [`crate::http::BlockingHttpTransport`]),
//!   forwards each call to it, and appends the observed interaction
//!   to a cassette. Sensitive headers (`Authorization`, `Cookie`, …)
//!   are redacted before they hit disk, so a recorded cassette can be
//!   committed without leaking the sandbox token used to record it.
//!
//! The two halves give connectors a record-once / replay-forever
//! workflow: a maintainer records against a provider sandbox once,
//! scrubs and commits the cassette, and from then on the replay test
//! proves the connector still shapes its requests and decodes the
//! provider's real response bytes — without any live credentials in
//! CI.
//!
//! This module is gated behind the `test-support` feature so it stays
//! out of production builds (it is a test scaffold, not a runtime
//! component).

use std::path::Path;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::error::{ConnectorError, Result};
use crate::http::{HttpMethod, HttpRequest, HttpResponse, HttpTransport};

/// Current on-disk cassette schema version. Bumped only on a
/// breaking change to the serialised shape; [`Cassette::load`]
/// rejects versions it does not understand so a stale fixture fails
/// loudly rather than mis-replaying.
pub const CASSETTE_VERSION: u32 = 1;

/// Header names (lower-cased) whose values are scrubbed to
/// [`REDACTED_PLACEHOLDER`] before a recorded interaction is written
/// to disk. Covers bearer tokens, cookies, and the common
/// provider-specific API-key headers so a committed cassette carries
/// no live credentials.
pub const REDACTED_HEADERS: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "cookie",
    "set-cookie",
    "x-api-key",
    "api-key",
    "x-auth-token",
    "x-amz-security-token",
    "x-goog-api-key",
];

/// Value substituted for a redacted header.
pub const REDACTED_PLACEHOLDER: &str = "REDACTED";

/// A request or response body.
///
/// JSON-API bodies are valid UTF-8, so the common case is
/// [`Body::Text`] — which keeps committed cassettes human-readable and
/// reviewable in a diff. Genuinely binary payloads fall back to
/// [`Body::Bytes`]. No body at all is represented by `Option::None` at
/// the call site (a 204 response, a GET request) rather than an empty
/// variant here.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Body {
    /// A UTF-8 text body, stored verbatim.
    Text(String),
    /// A non-UTF-8 byte body.
    Bytes(Vec<u8>),
}

impl Body {
    /// Wrap raw bytes, choosing [`Body::Text`] when the bytes are
    /// valid UTF-8 (the readable, common case) and [`Body::Bytes`]
    /// otherwise. Empty input yields `None` — there is no body to
    /// record.
    #[must_use]
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.is_empty() {
            return None;
        }
        match std::str::from_utf8(bytes) {
            Ok(s) => Some(Self::Text(s.to_string())),
            Err(_) => Some(Self::Bytes(bytes.to_vec())),
        }
    }

    /// Materialise the body back into raw bytes.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        match self {
            Self::Text(s) => s.clone().into_bytes(),
            Self::Bytes(b) => b.clone(),
        }
    }

    /// Render the optional body of an interaction as bytes, treating
    /// `None` as the empty body.
    #[must_use]
    pub fn opt_to_bytes(body: Option<&Self>) -> Vec<u8> {
        body.map(Self::to_bytes).unwrap_or_default()
    }
}

/// The request half of a recorded interaction.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CassetteRequest {
    /// Wire method.
    pub method: HttpMethod,
    /// Absolute request URL the connector issued.
    pub url: String,
    /// Request headers, post-redaction. Recorded for documentation /
    /// debugging only — [`ReplayTransport`] does **not** match on
    /// headers (the bearer token differs between record and replay).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<(String, String)>,
    /// Request body (`None` for GET / DELETE).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<Body>,
}

/// The response half of a recorded interaction.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CassetteResponse {
    /// HTTP status code.
    pub status: u16,
    /// Response headers (post-redaction). Replayed verbatim so the
    /// connector sees the provider's real `Link`, `Retry-After`, and
    /// `content-type` headers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<(String, String)>,
    /// Response body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<Body>,
}

/// One recorded request/response pair.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpInteraction {
    /// The request the connector issued.
    pub request: CassetteRequest,
    /// The response the provider returned.
    pub response: CassetteResponse,
}

/// A cassette — an ordered list of HTTP interactions plus metadata.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cassette {
    /// Schema version (see [`CASSETTE_VERSION`]).
    pub version: u32,
    /// Provider tag (e.g. `"github"`). Informational.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub provider: String,
    /// Free-text description of what the cassette captures.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// The recorded interactions, in capture order.
    pub interactions: Vec<HttpInteraction>,
}

impl Cassette {
    /// Construct an empty cassette tagged for `provider`.
    #[must_use]
    pub fn new(provider: impl Into<String>) -> Self {
        Self {
            version: CASSETTE_VERSION,
            provider: provider.into(),
            description: String::new(),
            interactions: Vec::new(),
        }
    }

    /// Attach a human-readable description (builder style).
    #[must_use]
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = description.into();
        self
    }

    /// Parse a cassette from a JSON string.
    ///
    /// # Errors
    ///
    /// Returns [`ConnectorError::Json`] if the JSON is malformed and
    /// [`ConnectorError::Sync`] if the schema version is unsupported.
    pub fn from_json(json: &str) -> Result<Self> {
        let cassette: Self = serde_json::from_str(json)?;
        if cassette.version != CASSETTE_VERSION {
            return Err(ConnectorError::Sync(format!(
                "cassette schema version {} is unsupported (this build expects {CASSETTE_VERSION})",
                cassette.version
            )));
        }
        Ok(cassette)
    }

    /// Load a cassette from a JSON file on disk.
    ///
    /// # Errors
    ///
    /// Returns [`ConnectorError::Transport`] if the file cannot be
    /// read, propagating the same error categories as
    /// [`Self::from_json`] otherwise.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let json = std::fs::read_to_string(path).map_err(|e| {
            ConnectorError::Transport(format!("cassette read {}: {e}", path.display()))
        })?;
        Self::from_json(&json)
    }

    /// Serialise the cassette to pretty JSON (stable, diff-friendly).
    ///
    /// # Errors
    ///
    /// Returns [`ConnectorError::Json`] if serialisation fails.
    pub fn to_json(&self) -> Result<String> {
        Ok(serde_json::to_string_pretty(self)?)
    }

    /// Write the cassette to a JSON file on disk.
    ///
    /// # Errors
    ///
    /// Returns [`ConnectorError::Transport`] if the file cannot be
    /// written.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        let json = self.to_json()?;
        std::fs::write(path, json).map_err(|e| {
            ConnectorError::Transport(format!("cassette write {}: {e}", path.display()))
        })
    }
}

/// Redact the values of sensitive headers in place, matching names
/// case-insensitively against [`REDACTED_HEADERS`].
fn redact_headers(headers: &[(String, String)]) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(name, value)| {
            let lower = name.to_ascii_lowercase();
            if REDACTED_HEADERS.contains(&lower.as_str()) {
                (name.clone(), REDACTED_PLACEHOLDER.to_string())
            } else {
                (name.clone(), value.clone())
            }
        })
        .collect()
}

// ───────────── ReplayTransport ─────────────

/// An [`HttpTransport`] that serves responses from a [`Cassette`]
/// instead of hitting the network.
///
/// Matching is by `(method, url)`. When a cassette holds several
/// interactions sharing the same `(method, url)` (e.g. the two pages
/// of a paginated list that happen to use the same URL), they are
/// consumed first-in-first-out: the first matching live request gets
/// the first un-consumed recorded interaction, and so on. Pagination
/// that varies a query parameter (`?page=2`) produces distinct URLs
/// and matches positionally regardless of order.
///
/// Every request the connector issues is also captured (see
/// [`Self::recorded_requests`]) so a test can assert on the exact
/// wire shape the connector produced, exactly like
/// [`crate::http::MockHttpTransport`].
#[derive(Debug)]
pub struct ReplayTransport {
    interactions: Vec<HttpInteraction>,
    consumed: Mutex<Vec<bool>>,
    recorded: Mutex<Vec<HttpRequest>>,
}

impl ReplayTransport {
    /// Build a replay transport from an in-memory cassette.
    #[must_use]
    pub fn new(cassette: Cassette) -> Self {
        let len = cassette.interactions.len();
        Self {
            interactions: cassette.interactions,
            consumed: Mutex::new(vec![false; len]),
            recorded: Mutex::new(Vec::new()),
        }
    }

    /// Convenience: load a cassette from disk and wrap it.
    ///
    /// # Errors
    ///
    /// Propagates [`Cassette::load`] errors.
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self> {
        Ok(Self::new(Cassette::load(path)?))
    }

    /// Convenience: parse a cassette from JSON and wrap it. Handy
    /// with `include_str!` so a fixture is baked into the test binary
    /// and needs no runtime path resolution.
    ///
    /// # Errors
    ///
    /// Propagates [`Cassette::from_json`] errors.
    pub fn from_json(json: &str) -> Result<Self> {
        Ok(Self::new(Cassette::from_json(json)?))
    }

    /// The requests the connector issued against this transport, in
    /// call order.
    #[must_use]
    pub fn recorded_requests(&self) -> Vec<HttpRequest> {
        self.recorded.lock().expect("replay recorded lock").clone()
    }

    /// Number of recorded interactions that were never matched by a
    /// live request.
    #[must_use]
    pub fn unplayed_count(&self) -> usize {
        self.consumed
            .lock()
            .expect("replay consumed lock")
            .iter()
            .filter(|played| !**played)
            .count()
    }

    /// Assert that every recorded interaction was consumed exactly
    /// once — a guard against fixtures that have drifted ahead of the
    /// connector (recorded calls the connector no longer makes).
    ///
    /// # Panics
    ///
    /// Panics, listing the unplayed interactions, if any remain.
    pub fn assert_all_played(&self) {
        let consumed = self.consumed.lock().expect("replay consumed lock");
        let leftover: Vec<String> = consumed
            .iter()
            .enumerate()
            .filter(|(_, played)| !**played)
            .map(|(i, _)| {
                let req = &self.interactions[i].request;
                format!("{} {}", req.method.as_str(), req.url)
            })
            .collect();
        assert!(
            leftover.is_empty(),
            "cassette has {} unplayed interaction(s): {leftover:?}",
            leftover.len()
        );
    }

    /// Find the index of the first un-consumed interaction matching
    /// `(method, url)`.
    fn next_match(&self, method: HttpMethod, url: &str) -> Option<usize> {
        let mut consumed = self.consumed.lock().expect("replay consumed lock");
        for (i, interaction) in self.interactions.iter().enumerate() {
            if consumed[i] {
                continue;
            }
            if interaction.request.method == method && interaction.request.url == url {
                consumed[i] = true;
                return Some(i);
            }
        }
        None
    }
}

impl HttpTransport for ReplayTransport {
    fn execute(&self, request: HttpRequest) -> Result<HttpResponse> {
        self.recorded
            .lock()
            .expect("replay recorded lock")
            .push(request.clone());

        let Some(idx) = self.next_match(request.method, &request.url) else {
            return Err(ConnectorError::Transport(format!(
                "cassette has no recorded interaction for {} {} \
                 (the fixture is stale or the connector issued an \
                 unexpected request)",
                request.method.as_str(),
                request.url,
            )));
        };
        let recorded = &self.interactions[idx].response;
        Ok(HttpResponse {
            status: recorded.status,
            headers: recorded.headers.clone(),
            body: Body::opt_to_bytes(recorded.body.as_ref()),
        })
    }
}

// ───────────── RecordingTransport ─────────────

/// An [`HttpTransport`] that forwards every call to an inner
/// transport and records the observed interactions into a
/// [`Cassette`], redacting sensitive headers along the way.
///
/// Wrap a reqwest-backed [`crate::http::BlockingHttpTransport`] in
/// this, point the connector at a provider sandbox, drive the
/// lifecycle once, then call [`Self::cassette`] / [`Self::save`] to
/// emit a fixture ready to commit and replay.
pub struct RecordingTransport {
    inner: Arc<dyn HttpTransport>,
    cassette: Mutex<Cassette>,
}

impl std::fmt::Debug for RecordingTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecordingTransport")
            .field("inner", &"<HttpTransport>")
            .field("cassette", &self.cassette)
            .finish()
    }
}

impl RecordingTransport {
    /// Wrap `inner`, tagging recorded interactions for `provider`.
    #[must_use]
    pub fn new(provider: impl Into<String>, inner: Arc<dyn HttpTransport>) -> Self {
        Self {
            inner,
            cassette: Mutex::new(Cassette::new(provider)),
        }
    }

    /// Snapshot the cassette recorded so far.
    #[must_use]
    pub fn cassette(&self) -> Cassette {
        self.cassette
            .lock()
            .expect("recording cassette lock")
            .clone()
    }

    /// Write the recorded cassette to disk.
    ///
    /// # Errors
    ///
    /// Propagates [`Cassette::save`] errors.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        self.cassette().save(path)
    }
}

impl HttpTransport for RecordingTransport {
    fn execute(&self, request: HttpRequest) -> Result<HttpResponse> {
        let recorded_request = CassetteRequest {
            method: request.method,
            url: request.url.clone(),
            headers: redact_headers(&request.headers),
            body: Body::from_bytes(&request.body),
        };
        let response = self.inner.execute(request)?;
        let recorded_response = CassetteResponse {
            status: response.status,
            headers: redact_headers(&response.headers),
            body: Body::from_bytes(&response.body),
        };
        self.cassette
            .lock()
            .expect("recording cassette lock")
            .interactions
            .push(HttpInteraction {
                request: recorded_request,
                response: recorded_response,
            });
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn interaction(method: HttpMethod, url: &str, status: u16, body: &str) -> HttpInteraction {
        HttpInteraction {
            request: CassetteRequest {
                method,
                url: url.to_string(),
                headers: Vec::new(),
                body: None,
            },
            response: CassetteResponse {
                status,
                headers: vec![("content-type".into(), "application/json".into())],
                body: Body::from_bytes(body.as_bytes()),
            },
        }
    }

    #[test]
    fn body_round_trips_text_and_bytes() {
        assert_eq!(Body::from_bytes(b""), None);
        assert_eq!(
            Body::from_bytes(b"hello"),
            Some(Body::Text("hello".to_string()))
        );
        let non_utf8 = [0xff, 0xfe, 0x00];
        assert_eq!(
            Body::from_bytes(&non_utf8),
            Some(Body::Bytes(non_utf8.to_vec()))
        );
        assert_eq!(Body::Text("x".into()).to_bytes(), b"x");
    }

    #[test]
    fn cassette_json_round_trip() {
        let mut cassette = Cassette::new("test").with_description("round-trip");
        cassette.interactions.push(interaction(
            HttpMethod::Get,
            "https://api/x",
            200,
            r#"{"a":1}"#,
        ));
        let json = cassette.to_json().expect("serialise");
        let parsed = Cassette::from_json(&json).expect("parse");
        assert_eq!(cassette, parsed);
    }

    #[test]
    fn rejects_unsupported_version() {
        let json = r#"{"version":999,"interactions":[]}"#;
        let err = Cassette::from_json(json).unwrap_err();
        assert!(matches!(err, ConnectorError::Sync(_)));
    }

    #[test]
    fn replay_serves_recorded_response() {
        let mut cassette = Cassette::new("test");
        cassette.interactions.push(interaction(
            HttpMethod::Get,
            "https://api/items?page=1",
            200,
            r#"{"page":1}"#,
        ));
        let transport = ReplayTransport::new(cassette);
        let resp = transport
            .execute(HttpRequest::get("https://api/items?page=1"))
            .expect("replay");
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, br#"{"page":1}"#);
        assert_eq!(resp.header("content-type"), Some("application/json"));
        transport.assert_all_played();
        assert_eq!(transport.recorded_requests().len(), 1);
    }

    #[test]
    fn replay_consumes_duplicate_urls_fifo() {
        let mut cassette = Cassette::new("test");
        cassette
            .interactions
            .push(interaction(HttpMethod::Post, "https://api/q", 200, "first"));
        cassette.interactions.push(interaction(
            HttpMethod::Post,
            "https://api/q",
            200,
            "second",
        ));
        let transport = ReplayTransport::new(cassette);

        let a = transport
            .execute(HttpRequest::post("https://api/q", b"{}".to_vec()))
            .expect("first");
        let b = transport
            .execute(HttpRequest::post("https://api/q", b"{}".to_vec()))
            .expect("second");
        assert_eq!(a.body, b"first");
        assert_eq!(b.body, b"second");
        transport.assert_all_played();
    }

    #[test]
    fn replay_errors_on_unrecorded_request() {
        let transport = ReplayTransport::new(Cassette::new("test"));
        let err = transport
            .execute(HttpRequest::get("https://api/missing"))
            .unwrap_err();
        assert!(matches!(err, ConnectorError::Transport(_)));
    }

    #[test]
    fn replay_reports_unplayed_interactions() {
        let mut cassette = Cassette::new("test");
        cassette
            .interactions
            .push(interaction(HttpMethod::Get, "https://api/a", 200, "a"));
        cassette
            .interactions
            .push(interaction(HttpMethod::Get, "https://api/b", 200, "b"));
        let transport = ReplayTransport::new(cassette);
        transport
            .execute(HttpRequest::get("https://api/a"))
            .expect("a");
        assert_eq!(transport.unplayed_count(), 1);
    }

    #[test]
    fn recording_wraps_inner_and_redacts_auth() {
        let mut cassette = Cassette::new("inner");
        cassette.interactions.push(HttpInteraction {
            request: CassetteRequest {
                method: HttpMethod::Get,
                url: "https://api/secure".to_string(),
                headers: Vec::new(),
                body: None,
            },
            response: CassetteResponse {
                status: 200,
                headers: vec![("set-cookie".into(), "session=abc".into())],
                body: Body::from_bytes(b"ok"),
            },
        });
        let inner: Arc<dyn HttpTransport> = Arc::new(ReplayTransport::new(cassette));
        let recorder = RecordingTransport::new("github", inner);

        let req = HttpRequest::get("https://api/secure").with_bearer("super-secret-token");
        let resp = recorder.execute(req).expect("record");
        assert_eq!(resp.status, 200);

        let recorded = recorder.cassette();
        assert_eq!(recorded.provider, "github");
        assert_eq!(recorded.interactions.len(), 1);
        // Request Authorization header is redacted.
        let auth = recorded.interactions[0]
            .request
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
            .map(|(_, v)| v.as_str());
        assert_eq!(auth, Some(REDACTED_PLACEHOLDER));
        // Response Set-Cookie header is redacted.
        let cookie = recorded.interactions[0]
            .response
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("set-cookie"))
            .map(|(_, v)| v.as_str());
        assert_eq!(cookie, Some(REDACTED_PLACEHOLDER));
    }
}
