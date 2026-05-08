//! `connector_framework` — Phase 4 connector boundary for the
//! Knowledge substrate.
//!
//! Per `PROPOSAL.md` §10.2 and `ARCHITECTURE.md` §4.1 every external
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
//! Phase 4 ships only the framework — the individual connectors
//! (Google Drive, OneDrive, Notion, Jira, …) land in later phases
//! via their own crates implementing [`Connector`].

#![deny(missing_docs)]

pub mod acl_sync;
pub mod attachment;
pub mod config;
pub mod connector;
pub mod error;
pub mod event;
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
pub use sync::{SyncMode, SyncState, SyncStatus};
pub use token_vault::{
    ConnectorInstanceId, OAuth2Token, OAuth2TokenVault, RefreshedToken, SecretToken, TokenRefresher,
};
pub use webhook::{
    parse_webhook_event, WebhookEventTypes, WebhookId, WebhookSecret, WebhookStatus,
    WebhookSubscription,
};
