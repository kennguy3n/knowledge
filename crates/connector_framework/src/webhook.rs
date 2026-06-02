//! Webhook subscriptions and event parsing.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::error::{ConnectorError, Result};
use crate::event::ConnectorEvent;
use crate::token_vault::ConnectorInstanceId;

/// Identifier for a [`WebhookSubscription`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WebhookId(pub Uuid);

impl WebhookId {
    /// Generate a fresh id.
    pub fn new_v4() -> Self {
        Self(Uuid::new_v4())
    }

    /// Borrow the underlying [`Uuid`].
    pub fn as_uuid(&self) -> Uuid {
        self.0
    }
}

impl std::fmt::Display for WebhookId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// A shared secret used to verify inbound webhook bodies via HMAC.
/// Zeroises on drop.
#[derive(Clone, Zeroize, ZeroizeOnDrop, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WebhookSecret(String);

impl WebhookSecret {
    /// Wrap a secret string.
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// Borrow the underlying secret. Callers must not log the result.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for WebhookSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("WebhookSecret").field(&"[redacted]").finish()
    }
}

/// Lifecycle of a webhook subscription.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebhookStatus {
    /// Subscribed and verified — events will be delivered.
    Active,
    /// Subscription is being installed; not yet verified.
    Pending,
    /// Provider has paused / disabled the subscription.
    Paused,
    /// Provider has invalidated the subscription; needs re-create.
    Expired,
}

/// Set of event categories a webhook subscribes to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebhookEventTypes {
    /// Subscribe to `document.created`.
    pub document_created: bool,
    /// Subscribe to `document.updated`.
    pub document_updated: bool,
    /// Subscribe to `document.deleted`.
    pub document_deleted: bool,
    /// Subscribe to `permission.changed`.
    pub permission_changed: bool,
}

impl WebhookEventTypes {
    /// Subscribe to every category — convenient default for simple
    /// connectors.
    pub fn all() -> Self {
        Self {
            document_created: true,
            document_updated: true,
            document_deleted: true,
            permission_changed: true,
        }
    }
}

impl Default for WebhookEventTypes {
    fn default() -> Self {
        Self::all()
    }
}

/// One webhook subscription owned by a connector instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebhookSubscription {
    /// Substrate-side id (independent of the provider's id).
    pub id: WebhookId,
    /// Connector this subscription belongs to.
    pub connector: ConnectorInstanceId,
    /// Public callback URL the provider POSTs to.
    pub callback_url: String,
    /// HMAC secret shared with the provider.
    pub secret: WebhookSecret,
    /// Event categories the subscription is registered for.
    pub event_types: WebhookEventTypes,
    /// Subscription lifecycle.
    pub status: WebhookStatus,
    /// Wall-clock creation time.
    pub created_at: DateTime<Utc>,
    /// Wall-clock expiry, when the provider issues finite-lifetime
    /// subscriptions (e.g. Google Drive — ~7 days max).
    pub expires_at: Option<DateTime<Utc>>,
    /// Provider-assigned subscription id (`Jira` webhook id, `Drive`
    /// channel id, `Microsoft Graph` subscription id, …). Used by the
    /// substrate to revoke / re-register the subscription on rotation.
    /// `None` for providers that don't issue server-side ids (Slack,
    /// Notion polling-only mode).
    #[serde(default)]
    pub provider_subscription_id: Option<String>,
}

impl WebhookSubscription {
    /// Construct a new subscription in [`WebhookStatus::Pending`].
    pub fn new(connector: ConnectorInstanceId,
        callback_url: impl Into<String>,
        secret: WebhookSecret,
        event_types: WebhookEventTypes,
        expires_at: Option<DateTime<Utc>>,
    ) -> Self {
        Self {
            id: WebhookId::new_v4(),
            connector,
            callback_url: callback_url.into(),
            secret,
            event_types,
            status: WebhookStatus::Pending,
            created_at: Utc::now(),
            expires_at,
            provider_subscription_id: None,
        }
    }

    /// Mark the subscription as verified and active.
    pub fn activate(&mut self) {
        self.status = WebhookStatus::Active;
    }

    /// Mark the subscription as paused / expired so callers can
    /// trigger a re-subscription flow.
    pub fn mark_status(&mut self, status: WebhookStatus) {
        self.status = status;
    }
}

/// Parse a JSON-encoded [`ConnectorEvent`] from a webhook body.
///
/// Returns [`ConnectorError::Webhook`] when the body is not valid
/// JSON (the upstream connector framework wraps body verification
/// around this; the parse step itself is just typed deserialisation).
pub fn parse_webhook_event(body: &[u8]) -> Result<ConnectorEvent> {
    serde_json::from_slice(body).map_err(|e| ConnectorError::Webhook(format!("parse error: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acl_sync::SourcePermissionLevel;
    use crate::event::{SourceDocumentId, SourceUserId};

    #[test]
    fn subscription_starts_pending_then_activates() {
        let mut s = WebhookSubscription::new(ConnectorInstanceId::new_v4(),
            "https://substrate.example/webhooks/abc",
            WebhookSecret::new("shared-secret"),
            WebhookEventTypes::all(),
            None,
        );
        assert_eq!(s.status, WebhookStatus::Pending);
        s.activate();
        assert_eq!(s.status, WebhookStatus::Active);
        s.mark_status(WebhookStatus::Expired);
        assert_eq!(s.status, WebhookStatus::Expired);
    }

    #[test]
    fn parses_document_created_event_from_json() {
        let payload = serde_json::json!({
            "type": "document_created",
            "document_id": "drive:abc",
            "occurred_at": Utc::now(),
        });
        let body = serde_json::to_vec(&payload).unwrap();
        let ev = parse_webhook_event(&body).unwrap();
        match ev {
            ConnectorEvent::DocumentCreated { document_id, .. } => {
                assert_eq!(document_id, SourceDocumentId::new("drive:abc"));
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn parses_permission_changed_with_revocation() {
        let payload = serde_json::json!({
            "type": "permission_changed",
            "document_id": "doc-3",
            "user_id": "u-7",
            "new_level": null,
            "occurred_at": Utc::now(),
        });
        let body = serde_json::to_vec(&payload).unwrap();
        let ev = parse_webhook_event(&body).unwrap();
        match ev {
            ConnectorEvent::PermissionChanged {
                document_id,
                user_id,
                new_level,
                ..
            } => {
                assert_eq!(document_id, SourceDocumentId::new("doc-3"));
                assert_eq!(user_id, SourceUserId::new("u-7"));
                assert!(new_level.is_none());
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn parses_permission_changed_with_grant() {
        let payload = serde_json::json!({
            "type": "permission_changed",
            "document_id": "doc-3",
            "user_id": "u-7",
            "new_level": "write",
            "occurred_at": Utc::now(),
        });
        let body = serde_json::to_vec(&payload).unwrap();
        let ev = parse_webhook_event(&body).unwrap();
        match ev {
            ConnectorEvent::PermissionChanged { new_level, .. } => {
                assert_eq!(new_level, Some(SourcePermissionLevel::Write));
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn invalid_json_yields_webhook_error() {
        let err = parse_webhook_event(b"not json").unwrap_err();
        assert!(matches!(err, ConnectorError::Webhook(_)));
    }

    #[test]
    fn debug_does_not_leak_secret() {
        let s = format!("{:?}", WebhookSecret::new("topsecret"));
        assert!(!s.contains("topsecret"));
    }
}
