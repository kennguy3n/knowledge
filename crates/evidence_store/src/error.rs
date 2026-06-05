//! Error type surfaced by the evidence store.

use thiserror::Error;

/// All errors produced by `knowledge_evidence_store`.
#[derive(Debug, Error)]
pub enum EvidenceError {
    /// Underlying SQLite / SQLCipher error.
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// Cryptographic error from the `crypto` crate.
    #[error("crypto error: {0}")]
    Crypto(#[from] crypto::CryptoError),

    /// The schema migration failed.
    #[error("schema migration failed: {0}")]
    Schema(&'static str),

    /// I/O error when opening the database file.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// The append-only invariant was violated (UPDATE / DELETE on
    /// `evidence`).
    #[error("evidence is append-only and cannot be {0}")]
    AppendOnlyViolation(&'static str),

    /// Evidence row could not be found.
    #[error("evidence not found: {0}")]
    NotFound(String),

    /// Body referenced by an evidence row is missing from the body
    /// store.
    #[error("body referenced by evidence row is missing from body_store")]
    DanglingBodyRef,

    /// Configuration is invalid (e.g. zero-byte ring buffer cap).
    #[error("invalid configuration: {0}")]
    InvalidConfig(&'static str),

    /// UTF-8 decoding failed when reading a stored text body.
    #[error("invalid utf-8 in stored body")]
    InvalidUtf8,

    /// Offline master-key rotation hit a failed precondition or
    /// integrity check (see [`crate::EvidenceStore::rotate_master_key`]).
    #[error("master-key rotation failed: {0}")]
    KeyRotation(String),

    /// An embedding model failed to embed a query or body. The
    /// payload preserves the underlying message so callers can
    /// attribute the failure without leaking memory via
    /// `Box::leak`-style static-string conversions.
    #[error("embedding error: {0}")]
    Embedding(String),
}

/// Convenience result alias.
pub type Result<T, E = EvidenceError> = std::result::Result<T, E>;
