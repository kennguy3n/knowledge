//! Error types for the relay server and its HTTP client.

/// Errors raised inside the relay server (storage, quota, routing).
#[derive(Debug, thiserror::Error)]
pub enum RelayError {
    /// A push would exceed the configured per-topic blob cap. The
    /// relay rejects the write rather than silently dropping earlier
    /// blobs, because dropping would invalidate the monotonic cursors
    /// replicas rely on. Operators raise the cap or back the relay
    /// with durable storage.
    #[error("topic blob quota exceeded (limit {limit} blobs)")]
    QuotaExceeded {
        /// Configured per-topic blob limit that was hit.
        limit: usize,
    },

    /// A single blob exceeded the configured byte cap. Caps the blast
    /// radius of a malformed or hostile client.
    #[error("sealed delta exceeds max blob size ({size} > {limit} bytes)")]
    BlobTooLarge {
        /// Size of the rejected blob in bytes.
        size: usize,
        /// Configured maximum blob size in bytes.
        limit: usize,
    },

    /// The TCP listener could not be bound (port in use, permission
    /// denied, …).
    #[error("relay bind failed: {0}")]
    Bind(String),

    /// The axum server returned an unrecoverable error while serving.
    #[error("relay serve failed: {0}")]
    Serve(String),
}

/// Errors raised by the HTTP [`SyncTransport`] client.
///
/// [`SyncTransport`]: sync_engine::SyncTransport
#[derive(Debug, thiserror::Error)]
pub enum HttpTransportError {
    /// The HTTP request itself failed (connection refused, timeout,
    /// DNS, TLS, …).
    #[error("relay request failed: {0}")]
    Request(String),

    /// The relay returned a non-success HTTP status.
    #[error("relay returned status {status}: {body}")]
    Status {
        /// HTTP status code returned by the relay.
        status: u16,
        /// Response body (diagnostic; never contains plaintext).
        body: String,
    },

    /// The relay's response body could not be decoded as the expected
    /// JSON shape.
    #[error("could not decode relay response: {0}")]
    Decode(String),
}
