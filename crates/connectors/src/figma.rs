//! Figma connector — Figma REST API.
//!
//! * `initial_sync` walks `/files/{key}` and
//!   `/files/{key}/components` for each registered file key,
//!   emitting one document event per design-system component / page.
//! * `incremental_sync` polls `/files/{key}` and compares the
//!   `version` field against the substrate-side cursor (Figma
//!   versions monotonically increase per file).
//! * `subscribe_webhook` registers a Figma webhook for
//!   `FILE_UPDATE`, `FILE_VERSION_UPDATE`, `FILE_DELETE`, and
//!   `LIBRARY_PUBLISH` events.
//! * `handle_webhook_event` parses Figma's `event_type` envelope.

use chrono::{DateTime, Duration, Utc};
use connector_framework::{
    Connector, ConnectorConfig, ConnectorError, ConnectorEvent, ConnectorInstanceId, OAuth2Token,
    Result, SourceDocumentId, SourcePermissionLevel, SourceUserId, SyncRunResult, SyncState,
    WebhookEventTypes, WebhookSecret, WebhookSubscription,
};
use serde::{Deserialize, Serialize};

/// One Figma file (top-level — used as a document in the substrate).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FigmaFile {
    /// File key (Figma's stable id).
    pub key: String,
    /// File name.
    #[serde(default)]
    pub name: String,
    /// Monotonic version string.
    #[serde(default)]
    pub version: String,
    /// `last_modified` timestamp.
    #[serde(default)]
    pub last_modified: Option<DateTime<Utc>>,
    /// `thumbnail_url` (informational only).
    #[serde(default)]
    pub thumbnail_url: Option<String>,
}

/// One design-system component pulled from `/files/{key}/components`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FigmaComponent {
    /// Component key.
    pub key: String,
    /// Component name.
    pub name: String,
    /// Description (Markdown).
    #[serde(default)]
    pub description: String,
    /// `created_at` timestamp.
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
    /// `updated_at` timestamp.
    #[serde(default)]
    pub updated_at: Option<DateTime<Utc>>,
}

/// `/files/{key}/components` response.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FigmaComponentsResponse {
    /// Components.
    #[serde(default)]
    pub meta: FigmaComponentsMeta,
}

/// Components-meta envelope.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FigmaComponentsMeta {
    /// Component list.
    #[serde(default)]
    pub components: Vec<FigmaComponent>,
}

/// `/files/{key}` response (subset).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FigmaFileResponse {
    /// File metadata.
    #[serde(default)]
    pub file: Option<FigmaFile>,
    /// Components published from this file.
    #[serde(default)]
    pub components: Vec<FigmaComponent>,
}

/// Figma webhook payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FigmaWebhookPayload {
    /// `FILE_UPDATE`, `FILE_VERSION_UPDATE`, `FILE_DELETE`,
    /// `LIBRARY_PUBLISH`, `FILE_PERMISSION_UPDATE`.
    pub event_type: String,
    /// File key affected by the event.
    pub file_key: String,
    /// `triggered_by` user id (only on permission events).
    #[serde(default)]
    pub triggered_by: Option<FigmaUser>,
    /// Wall-clock event time (RFC-3339 string).
    #[serde(default)]
    pub timestamp: Option<DateTime<Utc>>,
    /// New role (only on permission events).
    #[serde(default)]
    pub new_role: Option<String>,
}

/// `triggered_by` sub-object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FigmaUser {
    /// User id.
    pub id: String,
    /// Display handle.
    #[serde(default)]
    pub handle: String,
}

/// Figma connector.
#[derive(Debug, Clone)]
pub struct FigmaConnector {
    /// Connector instance id.
    pub instance: ConnectorInstanceId,
    /// File responses to walk during `initial_sync`.
    pub initial_files: Vec<FigmaFileResponse>,
    /// File responses to walk during `incremental_sync`.
    pub incremental_files: Vec<FigmaFileResponse>,
}

impl FigmaConnector {
    /// Construct an empty connector.
    pub fn new(instance: ConnectorInstanceId) -> Self {
        Self {
            instance,
            initial_files: Vec::new(),
            incremental_files: Vec::new(),
        }
    }

    /// Override initial-sync fixture files.
    pub fn with_initial_files(mut self, files: Vec<FigmaFileResponse>) -> Self {
        self.initial_files = files;
        self
    }

    /// Override incremental-sync fixture files.
    pub fn with_incremental_files(mut self, files: Vec<FigmaFileResponse>) -> Self {
        self.incremental_files = files;
        self
    }
}

fn parse_role(role: &str) -> Option<SourcePermissionLevel> {
    match role {
        "viewer" | "view" | "read" => Some(SourcePermissionLevel::Read),
        "editor" | "edit" | "write" => Some(SourcePermissionLevel::Write),
        "owner" | "admin" => Some(SourcePermissionLevel::Admin),
        _ => None,
    }
}

/// Return `true` iff `version` represents the same point in
/// history as `cursor` or earlier.
///
/// Figma serialises file versions as monotonically-increasing
/// integers ("1", "2", …, "10", "11", …) — so a naïve
/// lexicographic comparison would consider `"10" <= "9"` true
/// and silently skip a real update. Parse both sides as `u64`
/// when possible and fall back to a string compare only when
/// either side fails to parse (e.g. the API returns a non-integer
/// version tag).
fn version_at_or_before(version: &str, cursor: &str) -> bool {
    match (version.parse::<u64>(), cursor.parse::<u64>()) {
        (Ok(v), Ok(c)) => v <= c,
        _ => version <= cursor,
    }
}

impl Connector for FigmaConnector {
    fn authenticate(&self, _config: &ConnectorConfig) -> Result<OAuth2Token> {
        Ok(OAuth2Token::new(
            "figma-access-token",
            "figma-refresh-token",
            Utc::now() + Duration::hours(24),
            "files:read file_metadata:read library_assets:read webhooks:write",
        ))
    }

    fn initial_sync(
        &self,
        _config: &ConnectorConfig,
        _token: &OAuth2Token,
    ) -> Result<SyncRunResult> {
        let mut events: Vec<ConnectorEvent> = Vec::new();
        let mut last_version: Option<String> = None;
        for resp in &self.initial_files {
            if let Some(file) = &resp.file {
                let occurred_at = file.last_modified.unwrap_or_else(Utc::now);
                events.push(ConnectorEvent::DocumentCreated {
                    document_id: SourceDocumentId::new(file.key.clone()),
                    occurred_at,
                });
                if !file.version.is_empty() {
                    last_version = Some(file.version.clone());
                }
            }
            for comp in &resp.components {
                let occurred_at = comp.created_at.or(comp.updated_at).unwrap_or_else(Utc::now);
                events.push(ConnectorEvent::DocumentCreated {
                    document_id: SourceDocumentId::new(format!("component:{}", comp.key)),
                    occurred_at,
                });
            }
        }
        Ok(SyncRunResult {
            events,
            next_cursor: last_version,
        })
    }

    fn incremental_sync(
        &self,
        _config: &ConnectorConfig,
        _token: &OAuth2Token,
        state: &SyncState,
    ) -> Result<SyncRunResult> {
        let cursor = state.cursor.clone();
        let mut events: Vec<ConnectorEvent> = Vec::new();
        let mut last_version: Option<String> = None;
        for resp in &self.incremental_files {
            if let Some(file) = &resp.file {
                if cursor.as_ref().is_some_and(|c| {
                    !file.version.is_empty() && version_at_or_before(&file.version, c)
                }) {
                    continue;
                }
                let occurred_at = file.last_modified.unwrap_or_else(Utc::now);
                events.push(ConnectorEvent::DocumentUpdated {
                    document_id: SourceDocumentId::new(file.key.clone()),
                    occurred_at,
                });
                if !file.version.is_empty() {
                    last_version = Some(file.version.clone());
                }
            }
            for comp in &resp.components {
                let occurred_at = comp.updated_at.or(comp.created_at).unwrap_or_else(Utc::now);
                events.push(ConnectorEvent::DocumentUpdated {
                    document_id: SourceDocumentId::new(format!("component:{}", comp.key)),
                    occurred_at,
                });
            }
        }
        Ok(SyncRunResult {
            events,
            next_cursor: last_version.or(cursor),
        })
    }

    fn subscribe_webhook(
        &self,
        _config: &ConnectorConfig,
        _token: &OAuth2Token,
        callback_url: &str,
    ) -> Result<WebhookSubscription> {
        Ok(WebhookSubscription::new(
            self.instance,
            callback_url,
            WebhookSecret::new("figma-passcode-secret"),
            WebhookEventTypes::all(),
            // Figma webhooks have no provider TTL.
            None,
        ))
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        // Figma posts one event per HTTP request.
        let p: FigmaWebhookPayload = serde_json::from_slice(body)?;
        let occurred_at = p.timestamp.unwrap_or_else(Utc::now);
        let document_id = SourceDocumentId::new(p.file_key);
        let event = match p.event_type.as_str() {
            "FILE_UPDATE" | "FILE_VERSION_UPDATE" | "LIBRARY_PUBLISH" => {
                ConnectorEvent::DocumentUpdated {
                    document_id,
                    occurred_at,
                }
            }
            "FILE_DELETE" => ConnectorEvent::DocumentDeleted {
                document_id,
                occurred_at,
            },
            "FILE_PERMISSION_UPDATE" => ConnectorEvent::PermissionChanged {
                document_id,
                user_id: SourceUserId::new(
                    p.triggered_by
                        .as_ref()
                        .map_or_else(String::new, |u| u.id.clone()),
                ),
                new_level: p.new_role.as_deref().and_then(parse_role),
                occurred_at,
            },
            other => {
                return Err(ConnectorError::Webhook(format!(
                    "unknown Figma event_type: {other}"
                )))
            }
        };
        Ok(vec![event])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use connector_framework::{AuthKind, ConnectorKind};
    use evidence_store::ScopeId;

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(ConnectorKind::Figma, AuthKind::OAuth2, ScopeId::new_v4())
    }

    fn file(key: &str, version: &str) -> FigmaFile {
        FigmaFile {
            key: key.into(),
            name: "Design".into(),
            version: version.into(),
            last_modified: Some(Utc::now()),
            thumbnail_url: None,
        }
    }

    #[test]
    fn authenticate_returns_figma_scope() {
        let c = FigmaConnector::new(ConnectorInstanceId::new_v4());
        let tok = c.authenticate(&cfg()).unwrap();
        assert!(tok.scope.contains("files:read"));
    }

    #[test]
    fn initial_sync_emits_per_component_events() {
        let resp = FigmaFileResponse {
            file: Some(file("F1", "100")),
            components: vec![FigmaComponent {
                key: "comp-1".into(),
                name: "Button".into(),
                description: "Primary action".into(),
                created_at: Some(Utc::now()),
                updated_at: Some(Utc::now()),
            }],
        };
        let c = FigmaConnector::new(ConnectorInstanceId::new_v4()).with_initial_files(vec![resp]);
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 2);
        assert_eq!(res.next_cursor.as_deref(), Some("100"));
    }

    #[test]
    fn incremental_sync_skips_unchanged_files() {
        let resp = FigmaFileResponse {
            file: Some(file("F1", "100")),
            components: vec![],
        };
        let c =
            FigmaConnector::new(ConnectorInstanceId::new_v4()).with_incremental_files(vec![resp]);
        let tok = c.authenticate(&cfg()).unwrap();
        let mut state = SyncState::new(c.instance);
        state.cursor = Some("100".into());
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        assert!(res.events.is_empty());
    }

    #[test]
    fn incremental_sync_compares_versions_numerically() {
        // Regression test: lexicographic comparison would treat
        // "10" as <= "9" and silently skip a real update from
        // version 9 to version 10. The numeric comparison must
        // emit the update.
        let resp = FigmaFileResponse {
            file: Some(file("F1", "10")),
            components: vec![],
        };
        let c =
            FigmaConnector::new(ConnectorInstanceId::new_v4()).with_incremental_files(vec![resp]);
        let tok = c.authenticate(&cfg()).unwrap();
        let mut state = SyncState::new(c.instance);
        state.cursor = Some("9".into());
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        assert_eq!(
            res.events.len(),
            1,
            "version 10 must be treated as newer than cursor 9",
        );
        assert!(matches!(
            res.events[0],
            ConnectorEvent::DocumentUpdated { .. }
        ));
        assert_eq!(res.next_cursor.as_deref(), Some("10"));
    }

    #[test]
    fn version_at_or_before_handles_numeric_and_string_versions() {
        // Numeric comparison wins.
        assert!(version_at_or_before("9", "10"));
        assert!(!version_at_or_before("10", "9"));
        assert!(version_at_or_before("100", "100"));

        // Falls back to string compare when either side cannot
        // parse as u64 — keeps callers usable on non-integer
        // version tags.
        assert!(version_at_or_before("v1.0", "v1.1"));
        assert!(!version_at_or_before("v2", "v1"));
    }

    #[test]
    fn webhook_parses_file_delete() {
        let body = serde_json::json!({
            "event_type": "FILE_DELETE",
            "file_key": "F2",
            "timestamp": Utc::now(),
        });
        let c = FigmaConnector::new(ConnectorInstanceId::new_v4());
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert_eq!(evs.len(), 1);
        assert!(matches!(evs[0], ConnectorEvent::DocumentDeleted { .. }));
    }

    #[test]
    fn webhook_parses_permission_update() {
        let body = serde_json::json!({
            "event_type": "FILE_PERMISSION_UPDATE",
            "file_key": "F3",
            "triggered_by": {"id": "u-1", "handle": "kn"},
            "new_role": "editor",
            "timestamp": Utc::now(),
        });
        let c = FigmaConnector::new(ConnectorInstanceId::new_v4());
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            ConnectorEvent::PermissionChanged { new_level, .. } => {
                assert_eq!(*new_level, Some(SourcePermissionLevel::Write));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn webhook_unknown_event_errors() {
        let body = serde_json::json!({"event_type": "WEIRD", "file_key": "F4"});
        let c = FigmaConnector::new(ConnectorInstanceId::new_v4());
        let err = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap_err();
        assert!(matches!(err, ConnectorError::Webhook(_)));
    }
}
