//! `connector_framework` — connector boundary for the
//! Knowledge substrate.
//!
//! Per `docs/DESIGN.md` §10.2 and `ARCHITECTURE.md` §4.1 every external
//! source system the substrate ingests from sits behind one
//! [`Connector`] instance:
//!
//! * Authentication is handled via OAuth2; tokens are stored in
//!   [`OAuth2TokenVault`] and refreshed through the pluggable
//!   [`TokenRefresher`] hook.
//! * Sync runs round-trip provider cursors through [`SyncState`];
//!   the state machine carries the connector through full →
//!   incremental → failure → recovery transitions.
//! * Push subscriptions are described by [`WebhookSubscription`]
//!   and parsed via [`parse_webhook_event`].
//! * Each connector is attached to exactly one substrate scope via
//!   [`ConnectorAttachment`] / [`AttachmentRegistry`]; permission
//!   to attach / detach is gated through `permission_service`.
//! * Source-system ACLs are projected into the substrate's
//!   relation graph by [`AclSyncEngine`].
//!
//! Ships only the framework — the individual connectors (Google
//! Drive, OneDrive, Notion, Jira, …) live in their own crates
//! implementing [`Connector`].

#![deny(missing_docs)]

// STABLE
pub mod acl_sync;
// STABLE
pub mod attachment;
// STABLE
pub mod config;
// STABLE
pub mod connector;
// STABLE
pub mod error;
// STABLE
pub mod event;
// STABLE
pub mod http;
// UNSTABLE — internal HTTP helpers; signatures may change.
#[doc(hidden)]
pub mod http_helpers;
// STABLE
pub mod oauth;
// UNSTABLE — internal rate-limiter; API may change.
#[doc(hidden)]
pub mod provider_rate_limiter;
// STABLE
pub mod sync;
// STABLE
pub mod token_vault;
// STABLE
pub mod webhook;

#[cfg(feature = "async-runtime")]
pub mod async_runtime;
#[cfg(feature = "async-http-client")]
pub mod http_async;
#[cfg(feature = "webhook-server")]
pub mod webhook_server;

// STABLE
pub use acl_sync::{
    AclSyncEngine, AclSyncReport, PermissionDelta, PermissionMapping, SourcePermission,
    SourcePermissionLevel, SourceRevocation,
};
// STABLE
pub use attachment::{AttachmentId, AttachmentRegistry, ConnectorAttachment};
// STABLE
pub use config::{AuthKind, ConnectorConfig, ConnectorInstance, ConnectorKind};
// STABLE
pub use connector::{Connector, SyncRunResult};
// STABLE
pub use error::{ConnectorError, Result};
// STABLE
pub use event::{ConnectorEvent, SourceDocumentId, SourceUserId};
#[cfg(feature = "http-client")]
pub use http::{BlockingHttpTransport, DEFAULT_HTTP_TIMEOUT_SECS};
// STABLE
pub use http::{
    HttpMethod, HttpRequest, HttpResponse, HttpTransport, RetryPolicy, DEFAULT_MAX_RETRY_AFTER,
};
#[cfg(any(test, feature = "test-support"))]
pub use http::{MockHttpTransport, MockResponse, RecordedRequest};
// UNSTABLE — internal HTTP helpers; signatures may change.
#[doc(hidden)]
pub use http_helpers::{
    bearer_get_json, bearer_post_form, bearer_post_json, classify_failure, encode_form,
    percent_encode_form_component, percent_encode_path_component,
};
#[cfg(feature = "http-client")]
pub use oauth::default_oauth_client;
// STABLE
pub use oauth::{
    ClientSecretResolver, ConfiguredRefresher, OAuth2Client, DEFAULT_OAUTH_TIMEOUT_SECS,
};
// Deprecated transport-suggestive alias, retained for one minor
// cycle so external consumers can migrate without an API break.
// New code should reach for `OAuth2Client` directly.
// STABLE (deprecated alias — will be removed in a future minor release).
#[allow(deprecated)]
pub use oauth::ReqwestOAuth2Client;
// UNSTABLE — internal rate-limiter; API may change.
#[doc(hidden)]
pub use provider_rate_limiter::{
    provider_key_for_url, ProviderPolicy, ProviderRateLimiter, DEFAULT_MAX_TOKENS,
    DEFAULT_REFILL_RATE_PER_SEC,
};
// STABLE
pub use sync::{SyncMode, SyncState, SyncStatus};
// STABLE
pub use token_vault::{
    ConnectorInstanceId, OAuth2CodeExchange, OAuth2Token, OAuth2TokenVault, RefreshedToken,
    SecretToken, TokenRefresher,
};
// STABLE
pub use webhook::{
    parse_webhook_event, WebhookEventTypes, WebhookId, WebhookSecret, WebhookStatus,
    WebhookSubscription,
};

#[cfg(feature = "async-runtime")]
pub use async_runtime::{AsyncConnector, AsyncHttpTransport, BlockingConnectorAdapter};
#[cfg(feature = "async-http-client")]
pub use http_async::ReqwestAsyncHttpTransport;
#[cfg(feature = "webhook-server")]
pub use webhook_server::{WebhookDispatch, WebhookDispatcher, WebhookServer, WebhookServerConfig};
