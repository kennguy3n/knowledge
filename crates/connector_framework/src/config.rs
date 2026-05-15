//! Connector configuration and runtime instance.

use chrono::Duration;
use evidence_store::ScopeId;
use serde::{Deserialize, Serialize};

use crate::sync::SyncState;
use crate::token_vault::ConnectorInstanceId;

/// The well-known source kinds the connector framework supports
/// (per `docs/DESIGN.md` §10.2). Kept as an enum so attachments and
/// citations can route by source without parsing free-form strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectorKind {
    /// Google Drive.
    GoogleDrive,
    /// Microsoft OneDrive / SharePoint.
    OneDrive,
    /// Notion workspace.
    Notion,
    /// Atlassian Jira.
    Jira,
    /// Atlassian Confluence.
    Confluence,
    /// GitHub repository / issues.
    GitHub,
    /// Slack workspace.
    Slack,
    /// Figma design files.
    Figma,
    /// HubSpot CRM.
    HubSpot,
    /// Email — Gmail / Microsoft Graph (provider variant carried in
    /// the connector's `auth_config_json`).
    Email,
    /// Generic webhook-only connector — no provider-specific
    /// behaviour beyond `subscribe_webhook`.
    GenericWebhook,
}

impl ConnectorKind {
    /// Stable string tag used for serialisation and metadata.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::GoogleDrive => "google_drive",
            Self::OneDrive => "onedrive",
            Self::Notion => "notion",
            Self::Jira => "jira",
            Self::Confluence => "confluence",
            Self::GitHub => "github",
            Self::Slack => "slack",
            Self::Figma => "figma",
            Self::HubSpot => "hubspot",
            Self::Email => "email",
            Self::GenericWebhook => "generic_webhook",
        }
    }
}

/// Auth strategy for a connector — the connector framework only
/// needs to know which flow to invoke; the provider-specific OAuth
/// client config is passed verbatim through `auth_config_json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthKind {
    /// OAuth2 with refresh tokens (Google Drive, Notion, …).
    OAuth2,
    /// Long-lived API key in a header.
    ApiKey,
    /// Webhook-only — the source pushes events; no auth required for
    /// the substrate to read.
    None,
}

/// Connector-level configuration — type, scope binding, auth,
/// sync interval. Stable across runs; persisted by the connector
/// runtime.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConnectorConfig {
    /// Source kind.
    pub kind: ConnectorKind,
    /// Authentication strategy.
    pub auth: AuthKind,
    /// Scope (channel / domain / tenant) the connector is bound to.
    /// Observations derived from the connector inherit this scope.
    pub scope_id: ScopeId,
    /// How often the substrate should run an incremental sync (in
    /// seconds; chosen as a chrono [`Duration`] for ergonomics).
    /// Connectors with active webhooks can set this very high — the
    /// poll is just a backstop.
    #[serde(with = "duration_seconds")]
    pub sync_interval: Duration,
    /// Provider-specific auth config (client id, redirect uri,
    /// scopes, …). Schema-flexible so callers can extend without a
    /// migration. Secret values (client secrets, API keys) MUST be
    /// stored separately in the token vault — this field is for
    /// non-sensitive configuration only.
    pub auth_config_json: serde_json::Value,
}

impl ConnectorConfig {
    /// Construct a fresh config.
    pub fn new(kind: ConnectorKind, auth: AuthKind, scope_id: ScopeId) -> Self {
        Self {
            kind,
            auth,
            scope_id,
            sync_interval: Duration::minutes(15),
            auth_config_json: serde_json::Value::Null,
        }
    }

    /// Override the sync interval.
    pub fn with_sync_interval(mut self, interval: Duration) -> Self {
        self.sync_interval = interval;
        self
    }

    /// Override the auth-config JSON.
    pub fn with_auth_config(mut self, auth_config: serde_json::Value) -> Self {
        self.auth_config_json = auth_config;
        self
    }
}

mod duration_seconds {
    use chrono::Duration;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        d.num_seconds().serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        let s = i64::deserialize(d)?;
        Ok(Duration::seconds(s))
    }
}

/// Runtime state for one connector — config + current sync state +
/// the id used to look up the OAuth2 token in the vault.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConnectorInstance {
    /// Stable instance id.
    pub id: ConnectorInstanceId,
    /// Static config (source kind, scope, auth).
    pub config: ConnectorConfig,
    /// Current sync state.
    pub sync_state: SyncState,
}

impl ConnectorInstance {
    /// Construct a fresh instance with `SyncMode::Full` /
    /// `SyncStatus::NeverRun` and a freshly-generated id.
    pub fn new(config: ConnectorConfig) -> Self {
        let id = ConnectorInstanceId::new_v4();
        Self {
            id,
            config,
            sync_state: SyncState::new(id),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_round_trips_through_json() {
        let cfg = ConnectorConfig::new(
            ConnectorKind::GoogleDrive,
            AuthKind::OAuth2,
            ScopeId::new_v4(),
        )
        .with_sync_interval(Duration::minutes(30))
        .with_auth_config(serde_json::json!({"client_id": "abc"}));
        let s = serde_json::to_string(&cfg).unwrap();
        let back: ConnectorConfig = serde_json::from_str(&s).unwrap();
        assert_eq!(cfg, back);
    }

    #[test]
    fn instance_carries_consistent_ids() {
        let cfg = ConnectorConfig::new(ConnectorKind::Notion, AuthKind::OAuth2, ScopeId::new_v4());
        let inst = ConnectorInstance::new(cfg.clone());
        assert_eq!(inst.sync_state.connector, inst.id);
        assert_eq!(inst.config, cfg);
    }

    #[test]
    fn connector_kind_string_tag_is_stable() {
        assert_eq!(ConnectorKind::GoogleDrive.as_str(), "google_drive");
        assert_eq!(ConnectorKind::Jira.as_str(), "jira");
    }
}
