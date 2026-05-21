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

pub mod acl_sync;
pub mod attachment;
pub mod config;
pub mod connector;
pub mod error;
pub mod event;
pub mod http;
pub mod oauth;
pub mod sync;
pub mod token_vault;
pub mod webhook;

pub use acl_sync::{
    AclSyncEngine, AclSyncReport, PermissionDelta, PermissionMapping, SourcePermission,
    SourcePermissionLevel, SourceRevocation,
};
pub use attachment::{AttachmentId, AttachmentRegistry, ConnectorAttachment};
pub use config::{AuthKind, ConnectorConfig, ConnectorInstance, ConnectorKind};
pub use connector::{Connector, SyncRunResult};
pub use error::{ConnectorError, Result};
pub use event::{ConnectorEvent, SourceDocumentId, SourceUserId};
#[cfg(feature = "http-client")]
pub use http::{BlockingHttpTransport, DEFAULT_HTTP_TIMEOUT_SECS};
pub use http::{
    HttpMethod, HttpRequest, HttpResponse, HttpTransport, RetryPolicy, DEFAULT_MAX_RETRY_AFTER,
};
#[cfg(any(test, feature = "test-support"))]
pub use http::{MockHttpTransport, MockResponse, RecordedRequest};
#[cfg(feature = "http-client")]
pub use oauth::default_oauth_client;
pub use oauth::{ConfiguredRefresher, ReqwestOAuth2Client, DEFAULT_OAUTH_TIMEOUT_SECS};
pub use sync::{SyncMode, SyncState, SyncStatus};
pub use token_vault::{
    ConnectorInstanceId, OAuth2CodeExchange, OAuth2Token, OAuth2TokenVault, RefreshedToken,
    SecretToken, TokenRefresher,
};
pub use webhook::{
    parse_webhook_event, WebhookEventTypes, WebhookId, WebhookSecret, WebhookStatus,
    WebhookSubscription,
};
