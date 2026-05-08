//! Error types for the connector framework.

use thiserror::Error;

/// Errors raised by the connector framework.
#[derive(Debug, Error)]
pub enum ConnectorError {
    /// Authentication failed (invalid credentials, expired refresh
    /// token, provider rejected the request).
    #[error("authentication failed: {0}")]
    Auth(String),

    /// An OAuth2 token refresh attempt failed.
    #[error("token refresh failed: {0}")]
    TokenRefresh(String),

    /// The requested token vault entry was not found.
    #[error("token vault entry not found")]
    TokenNotFound,

    /// Sync failed (transport, parse, or rate-limited error from the
    /// upstream provider).
    #[error("sync failed: {0}")]
    Sync(String),

    /// Webhook subscription / verification / delivery failure.
    #[error("webhook failure: {0}")]
    Webhook(String),

    /// A connector instance with the same id is already registered.
    #[error("connector instance already exists")]
    DuplicateConnector,

    /// A connector instance was looked up but is not registered.
    #[error("connector instance not found")]
    ConnectorNotFound,

    /// A connector is already attached to the requested scope and
    /// the registry enforces one connector per `(source_kind, scope)`.
    #[error("connector already attached for this source on the given scope")]
    DuplicateAttachment,

    /// The requested attachment was not found.
    #[error("attachment not found")]
    AttachmentNotFound,

    /// Permission denied attaching / detaching a connector — the
    /// caller does not hold `admin` or `editor` on the scope.
    #[error("permission denied: subject lacks admin/editor on scope")]
    PermissionDenied,

    /// A relation tuple operation failed in the underlying
    /// permission service.
    #[error(transparent)]
    Permission(#[from] permission_service::PermissionError),

    /// JSON (de)serialisation error from a webhook body or sync
    /// cursor.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

/// Convenience alias.
pub type Result<T, E = ConnectorError> = std::result::Result<T, E>;
