//! HTTP error mapping for the loopback API.
//!
//! Every handler returns `Result<T, ApiError>`. [`ApiError`] wraps an
//! [`ffi::FfiError`] and renders it as a JSON body plus an HTTP status
//! chosen from the error's discriminant so the Go tier can drive its
//! own retry / surface policy off the status code alone (without
//! parsing the body), while still receiving the structured error for
//! diagnostics.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use ffi::FfiError;

/// Wrapper that turns an [`FfiError`] (or an ad-hoc bad-request) into
/// an axum [`Response`].
#[derive(Debug)]
pub struct ApiError(pub FfiError);

impl From<FfiError> for ApiError {
    fn from(e: FfiError) -> Self {
        Self(e)
    }
}

impl ApiError {
    /// Build a `400 Bad Request`-class error from a free-form message
    /// (used for request-body validation failures that occur before
    /// any FFI call).
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self(FfiError::InvalidId {
            message: message.into(),
        })
    }

    /// The HTTP status this error maps to.
    pub fn status(&self) -> StatusCode {
        match &self.0 {
            FfiError::InvalidId { .. } | FfiError::InvalidQuery { .. } => StatusCode::BAD_REQUEST,
            FfiError::NotFound { .. } => StatusCode::NOT_FOUND,
            FfiError::Unimplemented { .. } => StatusCode::NOT_IMPLEMENTED,
            FfiError::Unavailable { .. } => StatusCode::SERVICE_UNAVAILABLE,
            FfiError::Throttled { .. } => StatusCode::TOO_MANY_REQUESTS,
            // The connector / inference attempt reached a live
            // subsystem but the upstream itself failed — a gateway
            // error from the loopback's perspective.
            FfiError::Connector { .. } | FfiError::InferenceFailure { .. } => {
                StatusCode::BAD_GATEWAY
            }
            // Evidence / memory / synthesis / crypto failures are
            // genuine internal faults.
            FfiError::Evidence { .. }
            | FfiError::Memory { .. }
            | FfiError::Synthesis { .. }
            | FfiError::Crypto { .. } => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.status();
        // `Retry-After` (seconds) on throttle so the Go tier's HTTP
        // client can honour backpressure without parsing the body.
        if let FfiError::Throttled { retry_after_ms, .. } = &self.0 {
            let secs = retry_after_ms.div_ceil(1000).max(1);
            return (status, [("retry-after", secs.to_string())], Json(&self.0)).into_response();
        }
        (status, Json(&self.0)).into_response()
    }
}

/// Convenience result alias used by every handler.
pub type ApiResult<T> = Result<T, ApiError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_kinds_to_expected_statuses() {
        let cases = [
            (
                FfiError::InvalidId {
                    message: "x".into(),
                },
                StatusCode::BAD_REQUEST,
            ),
            (
                FfiError::InvalidQuery {
                    message: "fts5: syntax error".into(),
                },
                StatusCode::BAD_REQUEST,
            ),
            (
                FfiError::NotFound {
                    kind: "scope".into(),
                    id: "y".into(),
                },
                StatusCode::NOT_FOUND,
            ),
            (
                FfiError::Unimplemented {
                    method: "fetch_content".into(),
                },
                StatusCode::NOT_IMPLEMENTED,
            ),
            (
                FfiError::Unavailable {
                    subsystem: "store".into(),
                },
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            (
                FfiError::Throttled {
                    subsystem: "synth".into(),
                    retry_after_ms: 250,
                },
                StatusCode::TOO_MANY_REQUESTS,
            ),
            (
                FfiError::Connector {
                    message: "z".into(),
                },
                StatusCode::BAD_GATEWAY,
            ),
            (
                FfiError::Evidence {
                    message: "z".into(),
                },
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
        ];
        for (err, want) in cases {
            assert_eq!(ApiError(err).status(), want);
        }
    }

    #[test]
    fn bad_request_helper_maps_to_400() {
        assert_eq!(
            ApiError::bad_request("nope").status(),
            StatusCode::BAD_REQUEST
        );
    }
}
