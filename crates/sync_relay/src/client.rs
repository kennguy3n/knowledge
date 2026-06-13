//! Synchronous HTTP [`SyncTransport`] client for the relay.
//!
//! A replica drives this from its own thread (matching the
//! synchronous [`SyncEngine`] API), so it uses reqwest's **blocking**
//! client rather than dragging the engine into an async runtime.
//!
//! [`SyncEngine`]: sync_engine::SyncEngine

use reqwest::blocking::Client;
use reqwest::StatusCode;

use sync_engine::transport::{PullPage, SealedDelta, SyncTransport, TopicId};

use crate::error::HttpTransportError;
use crate::wire::{PushRequest, PushResponse};

/// Default request timeout. Generous enough for a fleet sync over a
/// slow link, short enough that a wedged relay surfaces as an error
/// rather than a hang.
const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// A blocking HTTP client implementing [`SyncTransport`] against a
/// relay server.
///
/// Holds the relay base URL and the tenant bearer token. Cloneable:
/// the underlying reqwest client pools connections, so cloning shares
/// the pool.
#[derive(Debug, Clone)]
pub struct HttpRelayTransport {
    base_url: String,
    token: String,
    http: Client,
}

impl HttpRelayTransport {
    /// Build a transport for `base_url` (e.g. `http://127.0.0.1:8080`)
    /// authenticating with bearer `token`.
    ///
    /// # Errors
    ///
    /// [`HttpTransportError::Request`] if the reqwest client cannot be
    /// constructed.
    pub fn new(
        base_url: impl Into<String>,
        token: impl Into<String>,
    ) -> Result<Self, HttpTransportError> {
        let http = Client::builder()
            .timeout(std::time::Duration::from_secs(DEFAULT_TIMEOUT_SECS))
            .build()
            .map_err(|e| HttpTransportError::Request(e.to_string()))?;
        Ok(Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            token: token.into(),
            http,
        })
    }

    fn deltas_url(&self, topic: &TopicId) -> String {
        format!("{}/v1/topics/{}/deltas", self.base_url, topic.to_hex())
    }
}

impl SyncTransport for HttpRelayTransport {
    type Error = HttpTransportError;

    fn push(&self, topic: &TopicId, blobs: &[SealedDelta]) -> Result<u64, Self::Error> {
        let body = PushRequest {
            blobs: blobs.to_vec(),
        };
        let resp = self
            .http
            .post(self.deltas_url(topic))
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .map_err(|e| HttpTransportError::Request(e.to_string()))?;
        let resp = check_status(resp)?;
        let parsed: PushResponse = resp
            .json()
            .map_err(|e| HttpTransportError::Decode(e.to_string()))?;
        Ok(parsed.cursor)
    }

    fn pull(&self, topic: &TopicId, since: u64) -> Result<PullPage, Self::Error> {
        let url = format!("{}/{since}", self.deltas_url(topic));
        let resp = self
            .http
            .get(url)
            .bearer_auth(&self.token)
            .send()
            .map_err(|e| HttpTransportError::Request(e.to_string()))?;
        let resp = check_status(resp)?;
        let page: PullPage = resp
            .json()
            .map_err(|e| HttpTransportError::Decode(e.to_string()))?;
        Ok(page)
    }
}

/// Convert a non-2xx response into [`HttpTransportError::Status`],
/// passing 2xx responses through.
fn check_status(
    resp: reqwest::blocking::Response,
) -> Result<reqwest::blocking::Response, HttpTransportError> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let code = status.as_u16();
    let body = resp.text().unwrap_or_default();
    // Map the relay's auth rejection through verbatim so callers can
    // distinguish "wrong token" from "relay down".
    debug_assert!(status != StatusCode::OK);
    Err(HttpTransportError::Status { status: code, body })
}
