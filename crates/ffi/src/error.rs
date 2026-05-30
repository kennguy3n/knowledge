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
///
/// `#[uniffi::error(flat_error)]` is intentionally NOT used here:
/// the variants carry per-kind diagnostic fields (`message`,
/// `subsystem`, `method`, `kind` / `id`) that platform hosts
/// inspect to drive their recovery policy (UI surface for
/// `NotFound`, retry on `Unavailable`, etc.). The default
/// rich-error mapping preserves those fields across the Swift /
/// Kotlin bindings; `flat_error` would collapse them to the
/// `Display` string and lose the structure.
#[derive(Debug, Clone, PartialEq, Eq, Error, Serialize, Deserialize, uniffi::Error)]
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
    /// yet, llama-server not running). Hosts SHOULD treat this as a
    /// soft / retryable failure.
    #[error("subsystem unavailable: {subsystem}")]
    Unavailable {
        /// Name of the unavailable subsystem.
        subsystem: String,
    },

    /// The underlying inference adapter executed the request but
    /// the model itself produced an unusable result (grammar
    /// violation, timeout, transport failure mid-stream, …). This
    /// is **distinct** from [`Self::Unavailable`]: the adapter is
    /// present and reachable, but the *attempt* failed. Hosts
    /// SHOULD NOT silently retry the same prompt — the call path
    /// already exhausted the router's fallback ladder — and instead
    /// either surface the error to the user, log it for triage, or
    /// reduce the input window before retrying.
    ///
    /// Mapped from [`inference_router::RouterError::InferenceFailure`].
    /// Earlier revisions of the FFI surface collapsed this into
    /// `Unavailable`, which lost the semantic distinction hosts
    /// need to drive their own retry policy.
    #[error("inference failure: {message}")]
    InferenceFailure {
        /// Diagnostic from the underlying adapter (model name,
        /// HTTP status, JSON parse error, etc.).
        message: String,
    },

    /// Underlying connector framework failure — auth handshake,
    /// HTTP transport, sync, token vault, webhook, or attachment
    /// error from `connector_framework`. Distinct from
    /// [`Self::Unavailable`] (which is for missing /
    /// not-yet-initialised subsystems) because the connector
    /// subsystem IS present and reachable, but the *attempt*
    /// against it failed.
    ///
    /// Mapped from
    /// [`connector_framework::ConnectorError`]. The substrate
    /// flattens the rich connector-error union into this single
    /// FFI variant because host-side recovery is uniform across
    /// the underlying cases ("show error banner, offer retry"):
    /// the diagnostic message preserves the original case in
    /// human-readable form for telemetry.
    #[error("connector failure: {message}")]
    Connector {
        /// Diagnostic from the underlying connector framework.
        message: String,
    },

    /// A rate-shaping limiter at the FFI boundary rejected the
    /// call (Phase 10 Item 5). Hosts SHOULD wait `retry_after_ms`
    /// and retry the same call — the rejection is purely
    /// rate-driven, the request itself is valid.
    ///
    /// Distinct from [`Self::Unavailable`] (the subsystem is
    /// missing or not yet initialised) and [`Self::Synthesis`]
    /// (the algorithm itself failed): when `Throttled` is
    /// returned, the subsystem is present, the input is
    /// well-formed, and the host is simply calling it too
    /// quickly. Currently surfaced by
    /// [`crate::synthesis::trigger_server_synthesis`] when the
    /// global token bucket has no tokens; future rate-limited
    /// surfaces should reuse this variant rather than overloading
    /// `Unavailable`.
    #[error("subsystem `{subsystem}` throttled (retry after {retry_after_ms} ms)")]
    Throttled {
        /// Name of the rate-limited subsystem (e.g.
        /// `"synthesis_engine"`). Mirrors the
        /// [`Self::Unavailable::subsystem`] field for symmetry.
        subsystem: String,
        /// Milliseconds the host should wait before retrying.
        /// Derived from the token bucket's current deficit and
        /// refill rate — a fresh retry at the indicated time
        /// will have at least one token available (modulo other
        /// concurrent callers draining the bucket first).
        retry_after_ms: u64,
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
            Self::InferenceFailure { .. } => "InferenceFailure",
            Self::Connector { .. } => "Connector",
            Self::Throttled { .. } => "Throttled",
        }
    }
}

impl From<connector_framework::ConnectorError> for FfiError {
    /// Flatten a [`connector_framework::ConnectorError`] into the
    /// uniform [`FfiError::Connector`] variant. Hosts switch on the
    /// `Connector` discriminant tag and read the human-readable
    /// `message` for diagnostics — they should not try to recover
    /// per sub-case (auth vs. transport vs. sync). See the
    /// `FfiError::Connector` docs for the rationale.
    fn from(err: connector_framework::ConnectorError) -> Self {
        FfiError::Connector {
            message: err.to_string(),
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
            FfiError::InferenceFailure {
                message: "grammar mismatch".into(),
            },
            FfiError::Connector {
                message: "auth handshake failed".into(),
            },
            FfiError::Throttled {
                subsystem: "synthesis_engine".into(),
                retry_after_ms: 250,
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
        assert_eq!(
            FfiError::InferenceFailure {
                message: "x".into()
            }
            .kind(),
            "InferenceFailure"
        );
        assert_eq!(
            FfiError::Connector {
                message: "x".into()
            }
            .kind(),
            "Connector"
        );
        assert_eq!(
            FfiError::Throttled {
                subsystem: "x".into(),
                retry_after_ms: 250,
            }
            .kind(),
            "Throttled"
        );
    }

    /// Mapping [`connector_framework::ConnectorError`] into the FFI
    /// surface MUST always come through as
    /// [`FfiError::Connector`] — never collapse into another
    /// variant. This pins the contract so a future refactor that
    /// adds a sibling FFI variant (e.g. `ConnectorAuth`) doesn't
    /// silently re-route auth failures and break host-side switch
    /// arms.
    #[test]
    fn connector_error_maps_into_connector_variant() {
        // Exhaustive coverage of every `ConnectorError` variant.
        // The blanket `From` impl above stringifies via `Display`
        // and stuffs the result into `FfiError::Connector`, so the
        // test asserts the variant collapse for every input variant
        // — including the ones that don't carry a payload
        // (`TokenNotFound`, `ConnectorNotFound`, `DuplicateConnector`),
        // and the auth-adjacent ones that future connector code
        // could plausibly raise during a sync
        // (`TokenRefresh`, `Webhook`).
        let cases = [
            connector_framework::ConnectorError::Auth("bad code".into()),
            connector_framework::ConnectorError::TokenRefresh("refresh denied".into()),
            connector_framework::ConnectorError::TokenNotFound,
            connector_framework::ConnectorError::Sync("rate limited".into()),
            connector_framework::ConnectorError::Webhook("subscribe failed".into()),
            connector_framework::ConnectorError::DuplicateConnector,
            connector_framework::ConnectorError::ConnectorNotFound,
            connector_framework::ConnectorError::Transport("tls handshake".into()),
        ];
        for c in cases {
            let original_display = c.to_string();
            let mapped: FfiError = c.into();
            match mapped {
                FfiError::Connector { ref message } => {
                    assert_eq!(message, &original_display);
                }
                other => panic!("expected FfiError::Connector, got {other:?}"),
            }
            assert_eq!(mapped.kind(), "Connector");
        }
    }

    /// `FfiError::InferenceFailure` and `FfiError::Unavailable` MUST
    /// remain distinct kinds. They collapse into the same FFI variant
    /// in earlier revisions of the surface — pinning the
    /// discriminants here prevents a future refactor from
    /// re-collapsing them and losing the host-visible retry semantics.
    #[test]
    fn inference_failure_distinct_from_unavailable() {
        let unavailable = FfiError::Unavailable {
            subsystem: "x".into(),
        };
        let failure = FfiError::InferenceFailure {
            message: "x".into(),
        };
        assert_ne!(unavailable.kind(), failure.kind());
        // JSON shapes also differ — hosts switch on `kind` and the
        // detail field name (`subsystem` vs `message`).
        let u_json = serde_json::to_value(&unavailable).expect("serialise unavailable");
        let f_json = serde_json::to_value(&failure).expect("serialise failure");
        assert_eq!(u_json["kind"], "Unavailable");
        assert_eq!(f_json["kind"], "InferenceFailure");
        assert!(u_json["detail"].get("subsystem").is_some());
        assert!(f_json["detail"].get("message").is_some());
    }
}
