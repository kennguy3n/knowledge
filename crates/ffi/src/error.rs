//! FFI error type — uniform contract across iOS / Android / N-API.
//!
//! The substrate's internal crates expose richly-typed errors
//! (`EvidenceError`, `MemoryError`, `PipelineError`, `CryptoError`,
//! …) that are not directly bridge-friendly. This module collapses
//! the union into a stable, **wire-flat** enum that:
//!
//! * Has a finite, documented, version-pinned set of variants.
//! * Round-trips through serde for diagnostic logging on the host.
//! * Preserves enough structure for hosts to take recovery action
//!   (e.g. retry on `Unavailable`, surface `NotFound` to the UI,
//!   degrade to read-only on `Crypto`).

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Result alias used by every public function in this crate.
pub type FfiResult<T> = std::result::Result<T, FfiError>;

/// Bridge-friendly error union surfaced to platform hosts.
#[derive(Debug, Clone, PartialEq, Eq, Error, Serialize, Deserialize)]
#[serde(tag = "kind", content = "detail")]
pub enum FfiError {
    /// Method exists in the contract but is not yet implemented in
    /// this build. Hosts should treat this as a *soft* failure and
    /// skip the call rather than crash.
    #[error("method `{method}` is not implemented in this build")]
    Unimplemented {
        /// The Rust function name.
        method: String,
    },

    /// `scope_id` / evidence id / memory id was malformed (non-UUID,
    /// wrong length, …).
    #[error("invalid identifier: {message}")]
    InvalidId {
        /// Free-form diagnostic.
        message: String,
    },

    /// The requested object did not exist.
    #[error("not found: {kind}/{id}")]
    NotFound {
        /// The object kind (`evidence`, `memory`, `scope`, …).
        kind: String,
        /// The object's UUID-string id.
        id: String,
    },

    /// Underlying evidence-store failure (FTS, AEAD, schema, …).
    #[error("evidence store failure: {message}")]
    Evidence {
        /// Diagnostic from the underlying crate.
        message: String,
    },

    /// Underlying memory-manager failure.
    #[error("memory manager failure: {message}")]
    Memory {
        /// Diagnostic from the underlying crate.
        message: String,
    },

    /// Underlying synthesis-pipeline failure.
    #[error("synthesis pipeline failure: {message}")]
    Synthesis {
        /// Diagnostic from the underlying crate.
        message: String,
    },

    /// Underlying crypto failure (key generation, AEAD, signature).
    #[error("crypto failure: {message}")]
    Crypto {
        /// Diagnostic from the underlying crate.
        message: String,
    },

    /// Transient unavailability — the underlying subsystem is
    /// expected to recover on retry (e.g. ONNX runtime not loaded
    /// yet, llama-server not running).
    #[error("subsystem unavailable: {subsystem}")]
    Unavailable {
        /// Name of the unavailable subsystem.
        subsystem: String,
    },
}

impl FfiError {
    /// Return the discriminant tag — useful for hosts that want to
    /// switch on the error kind without parsing the JSON body.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Unimplemented { .. } => "Unimplemented",
            Self::InvalidId { .. } => "InvalidId",
            Self::NotFound { .. } => "NotFound",
            Self::Evidence { .. } => "Evidence",
            Self::Memory { .. } => "Memory",
            Self::Synthesis { .. } => "Synthesis",
            Self::Crypto { .. } => "Crypto",
            Self::Unavailable { .. } => "Unavailable",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unimplemented_message_includes_method_name() {
        let err = FfiError::Unimplemented {
            method: "ingest_message".into(),
        };
        assert!(err.to_string().contains("ingest_message"));
        assert_eq!(err.kind(), "Unimplemented");
    }

    #[test]
    fn invalid_id_message_includes_diagnostic() {
        let err = FfiError::InvalidId {
            message: "not a uuid".into(),
        };
        assert!(err.to_string().contains("not a uuid"));
        assert_eq!(err.kind(), "InvalidId");
    }

    #[test]
    fn not_found_message_includes_kind_and_id() {
        let err = FfiError::NotFound {
            kind: "evidence".into(),
            id: "abc".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("evidence"));
        assert!(msg.contains("abc"));
        assert_eq!(err.kind(), "NotFound");
    }

    #[test]
    fn json_round_trip_preserves_structure() {
        let cases = vec![
            FfiError::Unimplemented {
                method: "encrypt".into(),
            },
            FfiError::InvalidId {
                message: "bad".into(),
            },
            FfiError::NotFound {
                kind: "memory".into(),
                id: "x".into(),
            },
            FfiError::Evidence {
                message: "fts boom".into(),
            },
            FfiError::Memory {
                message: "decay boom".into(),
            },
            FfiError::Synthesis {
                message: "synth boom".into(),
            },
            FfiError::Crypto {
                message: "aead boom".into(),
            },
            FfiError::Unavailable {
                subsystem: "onnx".into(),
            },
        ];
        for original in cases {
            let s = serde_json::to_string(&original).unwrap();
            let back: FfiError = serde_json::from_str(&s).unwrap();
            assert_eq!(original, back);
        }
    }

    #[test]
    fn kind_strings_are_stable() {
        assert_eq!(
            FfiError::Unimplemented { method: "x".into() }.kind(),
            "Unimplemented"
        );
        assert_eq!(
            FfiError::InvalidId {
                message: "x".into()
            }
            .kind(),
            "InvalidId"
        );
        assert_eq!(
            FfiError::NotFound {
                kind: "x".into(),
                id: "y".into()
            }
            .kind(),
            "NotFound"
        );
        assert_eq!(
            FfiError::Evidence {
                message: "x".into()
            }
            .kind(),
            "Evidence"
        );
        assert_eq!(
            FfiError::Memory {
                message: "x".into()
            }
            .kind(),
            "Memory"
        );
        assert_eq!(
            FfiError::Synthesis {
                message: "x".into()
            }
            .kind(),
            "Synthesis"
        );
        assert_eq!(
            FfiError::Crypto {
                message: "x".into()
            }
            .kind(),
            "Crypto"
        );
        assert_eq!(
            FfiError::Unavailable {
                subsystem: "x".into()
            }
            .kind(),
            "Unavailable"
        );
    }
}
