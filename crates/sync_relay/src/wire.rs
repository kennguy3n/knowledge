//! HTTP wire DTOs shared by the relay server and the
//! [`HttpRelayTransport`] client.
//!
//! The pull response reuses [`sync_engine::transport::PullPage`]
//! verbatim — it is already `serde`-serialisable and is exactly the
//! shape the client's [`SyncTransport::pull`] returns — so there is
//! one canonical wire representation of a page of sealed deltas.
//!
//! [`HttpRelayTransport`]: crate::client::HttpRelayTransport
//! [`SyncTransport::pull`]: sync_engine::SyncTransport::pull

use serde::{Deserialize, Serialize};

use sync_engine::transport::SealedDelta;

/// Request body for `POST /v1/topics/{topic}/deltas`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushRequest {
    /// Opaque sealed deltas to append, in order.
    pub blobs: Vec<SealedDelta>,
}

/// Response body for `POST /v1/topics/{topic}/deltas`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct PushResponse {
    /// The topic's new high-water cursor after the append.
    pub cursor: u64,
}
