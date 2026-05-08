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

    /// An embedding model call failed (query embedding, per-row
    /// embedding, …). Carries an owned message so dynamic context can
    /// flow through without leaking memory via `Box::leak`.
    #[error("embedding error: {0}")]
    Embedding(String),
}

/// Convenience result alias.
pub type Result<T, E = EvidenceError> = std::result::Result<T, E>;
