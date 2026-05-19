//! N-API error type — JSON-stable wrapper around [`ffi::FfiError`].
//!
//! When the real `#[napi]` proc-macros are introduced, this
//! type's `From<ffi::FfiError>` impl is the single conversion point
//! that maps Rust errors into Node `Error` instances.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use ffi::FfiError;

/// Result alias used by the N-API addon surface.
pub type NapiResult<T> = std::result::Result<T, NapiError>;

/// JSON-stable error envelope for the desktop bridge.
#[derive(Debug, Clone, PartialEq, Eq, Error, Serialize, Deserialize)]
#[serde(tag = "kind", content = "detail")]
pub enum NapiError {
    /// `init(config_json)` rejected its input.
    #[error("invalid init config: {message}")]
    InvalidConfig {
        /// Diagnostic from the JSON parser or schema check.
        message: String,
    },

    /// A request argument failed validation (e.g. malformed base64,
    /// non-UUID scope id).
    #[error("invalid argument: {message}")]
    InvalidArgument {
        /// Diagnostic for the host.
        message: String,
    },

    /// Forwarded error from the underlying [`ffi`] surface.
    #[error("{0}")]
    Ffi(FfiError),
}

impl NapiError {
    /// Discriminant tag for stable wire matching.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::InvalidConfig { .. } => "InvalidConfig",
            Self::InvalidArgument { .. } => "InvalidArgument",
            Self::Ffi(inner) => inner.kind(),
        }
    }
}

impl From<FfiError> for NapiError {
    fn from(value: FfiError) -> Self {
        Self::Ffi(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_config_message_includes_diagnostic() {
        let err = NapiError::InvalidConfig {
            message: "expected object".into(),
        };
        assert!(err.to_string().contains("expected object"));
        assert_eq!(err.kind(), "InvalidConfig");
    }

    #[test]
    fn invalid_argument_message_includes_diagnostic() {
        let err = NapiError::InvalidArgument {
            message: "bad b64".into(),
        };
        assert!(err.to_string().contains("bad b64"));
        assert_eq!(err.kind(), "InvalidArgument");
    }

    #[test]
    fn ffi_error_forwards_through_kind() {
        let err: NapiError = FfiError::Unimplemented { method: "x".into() }.into();
        assert_eq!(err.kind(), "Unimplemented");
    }

    #[test]
    fn json_round_trip_preserves_structure() {
        let err = NapiError::InvalidConfig {
            message: "boom".into(),
        };
        let s = serde_json::to_string(&err).unwrap();
        let back: NapiError = serde_json::from_str(&s).unwrap();
        assert_eq!(err, back);
    }
}
