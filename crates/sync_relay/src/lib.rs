//! Authenticated, **untrusted** store-and-forward relay for the
//! Knowledge substrate's multi-device sync.
//!
//! # What the relay is
//!
//! A user's devices each run a [`sync_engine::SyncEngine`] and a
//! [`sync_engine::SyncClient`]. The client AEAD-seals each delta under
//! a per-scope key the relay never holds, then ships the opaque
//! [`SealedDelta`] to this relay, which buckets it by
//! `(tenant, topic)` and forwards it to the user's other devices on
//! demand. Convergence is a property of the CRDT merge in
//! `sync_engine`, **not** of the relay — so the relay is a dumb,
//! replaceable buffer.
//!
//! # What the relay is NOT
//!
//! * It is **not** a trusted authority. It cannot decrypt deltas
//!   (no master key), cannot link a [`TopicId`] back to a scope, and
//!   cannot resolve, reorder, or merge CRDT state. Its entire job is
//!   "append these opaque bytes" and "give me the opaque bytes after
//!   cursor N".
//! * It is **not** where conflict resolution happens. Add-wins +
//!   supersession are computed locally by every replica.
//!
//! # Multi-tenancy
//!
//! A bearer token authenticates each request and resolves to a
//! [`TenantId`]; blobs are stored under `(tenant, topic)`. This keeps
//! thousands of SME tenants isolated on one relay even though topics
//! are derived client-side and the relay cannot interpret them.
//!
//! # Layout
//!
//! * [`auth`] — bearer tokens → tenants.
//! * [`store`] — the [`BlobStore`] trait + in-memory reference impl.
//! * [`server`] — the axum HTTP server.
//! * [`client`] — the blocking HTTP [`SyncTransport`] client.
//! * [`wire`] — the HTTP request/response DTOs.
//!
//! [`SealedDelta`]: sync_engine::SealedDelta
//! [`TopicId`]: sync_engine::TopicId
//! [`SyncTransport`]: sync_engine::SyncTransport
//! [`BlobStore`]: crate::store::BlobStore

#![deny(missing_docs)]

pub mod auth;
pub mod client;
pub mod error;
pub mod server;
pub mod store;
pub mod wire;

pub use auth::{TenantId, TokenRegistry};
pub use client::HttpRelayTransport;
pub use error::{HttpTransportError, RelayError};
pub use server::{build_router, RelayConfig, RelayServer, RelayState};
pub use store::{BlobStore, InMemoryBlobStore, StoreLimits};
