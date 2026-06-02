//! Figma connector — Figma REST API.
//!
//! * `initial_sync` walks `/v1/files/{key}` for every file key the
//!   tenant configured (`auth_config_json.file_keys`), emitting one
//!   `DocumentCreated` event per file and per published design-system
//!   component. The next-cursor is a JSON map of per-file
//!   monotonic versions so the next incremental sync can detect new
//!   versions across heterogeneous fleets.
//! * `incremental_sync` re-polls the same `/v1/files/{key}` endpoints
//!   and only emits `DocumentUpdated` for files whose version moved
//!   past the prior cursor (Figma file versions are
//!   monotonically-increasing integers, with a string fallback for
//!   non-integer tags).
//! * `subscribe_webhook` POSTs `/v2/webhooks` for each configured
//!   team and captures the assigned webhook id in
//!   `WebhookSubscription::provider_subscription_id` so the substrate
//!   can revoke / re-register on rotation. The body includes the
//!   `passcode` Figma will echo back on every event for signature
//!   verification.
//! * `handle_webhook_event` parses Figma's `event_type` envelope —
//!   `FILE_UPDATE`, `FILE_VERSION_UPDATE`, `FILE_DELETE`,
//!   `LIBRARY_PUBLISH`, and `FILE_PERMISSION_UPDATE`.
//!
//! Production wiring runs over [`HttpTransport`] — the substrate
//! constructs a [`FigmaConnector`] with a real
//! `connector_framework::BlockingHttpTransport` (under the
//! `http-client` feature) and a real `OAuth2Client` for the
//! `https://www.figma.com/api/oauth/token` exchange. Unit tests pass
//! `MockHttpTransport` + a fixture OAuth2 exchange.

use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use connector_framework::{
    bearer_get_json, bearer_post_json, percent_encode_path_component, Connector, ConnectorConfig,
    ConnectorError, ConnectorEvent, ConnectorInstanceId, FetchedContent, HttpMethod, HttpRequest,
    HttpTransport, OAuth2CodeExchange, OAuth2Token, Result, SourceDocumentId,
    SourcePermissionLevel, SourceUserId, SyncRunResult, SyncState, WebhookEventTypes,
    WebhookSecret, WebhookSubscription,
};
use serde::{Deserialize, Serialize};

/// Default Figma REST base URL. Per-instance overrides go through
/// `auth_config_json.api_base_url`.
pub const DEFAULT_API_BASE_URL: &str = "https://api.figma.com";

/// One Figma file (top-level — used as a document in the substrate).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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

/// One design-system component pulled from `/v1/files/{key}` or
/// `/v1/files/{key}/components`.
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

/// `/v1/files/{key}` response (subset).
///
/// Figma's `/v1/files/{key}` returns file metadata at the root of
/// the JSON document plus a `components` map keyed by node id. We
/// flatten the components map into a list with `#[serde(deserialize_with)]`
/// to keep substrate code uniform regardless of whether the
/// transport happens to return a map or an array.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FigmaFileResponse {
    /// File metadata.
    #[serde(default)]
    pub file: Option<FigmaFile>,
    /// Components published from this file. Wire format is a JSON
    /// map; the substrate normalises to a list via
    /// [`FigmaFileResponseRaw`].
    #[serde(default)]
    pub components: Vec<FigmaComponent>,
}

/// Wire-format `/v1/files/{key}` response with the file fields at
/// the root and `components` as a map. Used by the connector to
/// normalise into [`FigmaFileResponse`].
#[derive(Debug, Clone, Default, Deserialize)]
struct FigmaFileResponseRaw {
    #[serde(default)]
    name: String,
    #[serde(default)]
    version: String,
    #[serde(default, rename = "lastModified")]
    last_modified: Option<DateTime<Utc>>,
    #[serde(default, rename = "thumbnailUrl")]
    thumbnail_url: Option<String>,
    #[serde(default)]
    components: BTreeMap<String, FigmaComponent>,
}

/// `GET /v1/images/{key}` response — maps node id → rendered image
/// URL (or `null` when Figma could not render that node).
#[derive(Debug, Clone, Default, Deserialize)]
struct FigmaImagesResponse {
    #[serde(default)]
    images: BTreeMap<String, Option<String>>,
    #[serde(default)]
    err: Option<String>,
}

/// Upper bound on the number of nodes we request rendered PNGs for in
/// a single `fetch_content` call — keeps the `ids=` query string and
/// the downstream render job bounded for very large files.
const MAX_RENDER_NODES: usize = 50;

/// Recursion ceiling for the Figma document-tree walkers. Figma files
/// nest only a few dozen levels in practice; this cap stops a
/// pathological or cyclic response from recursing without bound.
const MAX_TREE_DEPTH: usize = 256;

/// Recursively collect the `characters` of every `TEXT` node in the
/// Figma document tree, depth-first, preserving document order.
fn collect_text_nodes(node: &serde_json::Value, depth: usize, out: &mut Vec<String>) {
    if depth >= MAX_TREE_DEPTH {
        return;
    }
    if node.get("type").and_then(serde_json::Value::as_str) == Some("TEXT") {
        if let Some(chars) = node.get("characters").and_then(serde_json::Value::as_str) {
            if !chars.is_empty() {
                out.push(chars.to_string());
            }
        }
    }
    if let Some(children) = node.get("children").and_then(serde_json::Value::as_array) {
        for child in children {
            collect_text_nodes(child, depth + 1, out);
        }
    }
}

/// Recursively collect node ids worth rendering to PNG — top-level
/// frames, components and component sets. Stops once [`MAX_RENDER_NODES`]
/// ids have been gathered or [`MAX_TREE_DEPTH`] is reached.
fn collect_render_node_ids(node: &serde_json::Value, depth: usize, out: &mut Vec<String>) {
    if out.len() >= MAX_RENDER_NODES || depth >= MAX_TREE_DEPTH {
        return;
    }
    let node_type = node.get("type").and_then(serde_json::Value::as_str);
    if matches!(node_type, Some("FRAME" | "COMPONENT" | "COMPONENT_SET")) {
        if let Some(id) = node.get("id").and_then(serde_json::Value::as_str) {
            out.push(id.to_string());
        }
    }
    if let Some(children) = node.get("children").and_then(serde_json::Value::as_array) {
        for child in children {
            collect_render_node_ids(child, depth + 1, out);
        }
    }
}

/// Response from `POST /v2/webhooks`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FigmaWebhookCreateResponse {
    /// Numeric webhook id Figma assigned.
    #[serde(default)]
    pub id: Option<String>,
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
///
/// Per-tenant `file_keys` and `team_ids` are read from
/// `auth_config_json` on every sync — the substrate persists them
/// at install time. Production routes every REST call through
/// `transport`.
#[derive(Clone)]
pub struct FigmaConnector {
    /// Connector instance id.
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    oauth: Arc<dyn OAuth2CodeExchange>,
    api_base_url: String,
}

impl std::fmt::Debug for FigmaConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FigmaConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("transport", &"<HttpTransport>")
            .field("oauth", &"<OAuth2CodeExchange>")
            .finish()
    }
}

impl FigmaConnector {
    /// Construct a Figma connector.
    ///
    /// Production wires `transport` to `BlockingHttpTransport` and
    /// `oauth` to `OAuth2Client`; tests use `MockHttpTransport`.
    pub fn new(
        instance: ConnectorInstanceId,
        transport: Arc<dyn HttpTransport>,
        oauth: Arc<dyn OAuth2CodeExchange>,
    ) -> Self {
        Self {
            instance,
            transport,
            oauth,
            api_base_url: DEFAULT_API_BASE_URL.to_string(),
        }
    }

    /// Override the Figma REST base URL.
    #[must_use]
    pub fn with_api_base_url(mut self, url: impl Into<String>) -> Self {
        self.api_base_url = url.into();
        self
    }

    fn resolved_base_url(&self, config: &ConnectorConfig) -> String {
        config
            .auth_config_json
            .get("api_base_url")
            .and_then(serde_json::Value::as_str)
            .map_or_else(
                || self.api_base_url.clone(),
                std::string::ToString::to_string,
            )
    }

    fn configured_file_keys(config: &ConnectorConfig) -> Result<Vec<String>> {
        let raw = config
            .auth_config_json
            .get("file_keys")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| {
                ConnectorError::Sync(
                    "figma sync: auth_config_json.file_keys (array of file keys) is required"
                        .into(),
                )
            })?;
        let keys: Vec<String> = raw
            .iter()
            .filter_map(|v| v.as_str().map(std::string::ToString::to_string))
            .filter(|s| !s.is_empty())
            .collect();
        if keys.is_empty() {
            return Err(ConnectorError::Sync(
                "figma sync: auth_config_json.file_keys was present but empty".into(),
            ));
        }
        Ok(keys)
    }

    fn configured_team_ids(config: &ConnectorConfig) -> Result<Vec<String>> {
        let raw = config
            .auth_config_json
            .get("team_ids")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| {
                ConnectorError::Webhook(
                    "figma subscribe_webhook: auth_config_json.team_ids (array) is required".into(),
                )
            })?;
        let ids: Vec<String> = raw
            .iter()
            .filter_map(|v| v.as_str().map(std::string::ToString::to_string))
            .filter(|s| !s.is_empty())
            .collect();
        if ids.is_empty() {
            return Err(ConnectorError::Webhook(
                "figma subscribe_webhook: auth_config_json.team_ids was present but empty".into(),
            ));
        }
        Ok(ids)
    }

    fn fetch_file(
        &self,
        base_url: &str,
        token: &OAuth2Token,
        file_key: &str,
    ) -> Result<FigmaFileResponse> {
        let url = format!("{base_url}/v1/files/{file_key}");
        let raw: FigmaFileResponseRaw = bearer_get_json(
            &self.transport,
            "figma",
            "/v1/files/{key}",
            &url,
            token,
            &[],
        )?;
        let components: Vec<FigmaComponent> = raw.components.into_values().collect();
        Ok(FigmaFileResponse {
            file: Some(FigmaFile {
                key: file_key.to_string(),
                name: raw.name,
                version: raw.version,
                last_modified: raw.last_modified,
                thumbnail_url: raw.thumbnail_url,
            }),
            components,
        })
    }

    /// Best-effort revocation of already-registered webhook ids when
    /// `subscribe_webhook` aborts partway through the team_id ×
    /// event_type matrix. The caller has decided to fail the
    /// subscription, which means the registered ids will never be
    /// persisted in [`WebhookSubscription::provider_subscription_id`]
    /// — so without rollback they would orphan in Figma's dashboard,
    /// continuing to deliver webhooks that the substrate can't
    /// correlate.
    ///
    /// Cleanup is **best-effort**: each DELETE is attempted, and
    /// individual failures are swallowed (we already have an
    /// outer error to surface; a secondary failure shouldn't mask
    /// it). The connector emits no observable side effect beyond
    /// the DELETE requests on the transport. The original error is
    /// always preserved by the caller via `return Err(_)` after
    /// this method returns.
    fn rollback_partial_webhooks(&self, base_url: &str, token: &OAuth2Token, ids: &[String]) {
        for id in ids {
            let url = format!("{base_url}/v2/webhooks/{id}");
            // We could in principle parse the response to log /
            // surface secondary failures, but the goal here is to
            // tear down what we can without escalating. Each call
            // already retries on transient errors at the transport
            // layer; once that exhausts we accept the orphan and
            // move on.
            let req = HttpRequest {
                method: HttpMethod::Delete,
                url,
                headers: Vec::new(),
                body: Vec::new(),
            }
            .with_bearer(token.access_token.expose());
            let _ = self.transport.execute(req);
        }
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

/// Serialise the per-file watermark map as the substrate cursor.
fn encode_cursor(versions: &BTreeMap<String, String>) -> Option<String> {
    if versions.is_empty() {
        return None;
    }
    serde_json::to_string(versions).ok()
}

/// Parse the per-file watermark map from the substrate cursor.
fn decode_cursor(cursor: Option<&str>) -> BTreeMap<String, String> {
    cursor
        .and_then(|s| serde_json::from_str::<BTreeMap<String, String>>(s).ok())
        .unwrap_or_default()
}

impl Connector for FigmaConnector {
    fn authenticate(&self, config: &ConnectorConfig) -> Result<OAuth2Token> {
        let auth_code = config
            .auth_config_json
            .get("authorization_code")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ConnectorError::Auth(
                    "figma authenticate: auth_config_json.authorization_code is required".into(),
                )
            })?;
        self.oauth.exchange_code(config, auth_code)
    }

    fn initial_sync(&self, config: &ConnectorConfig, token: &OAuth2Token) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        let file_keys = Self::configured_file_keys(config)?;
        let mut events: Vec<ConnectorEvent> = Vec::new();
        let mut versions = BTreeMap::<String, String>::new();
        for file_key in &file_keys {
            let resp = self.fetch_file(&base_url, token, file_key)?;
            let file = resp.file.unwrap_or_default();
            let occurred_at = file.last_modified.unwrap_or_else(Utc::now);
            events.push(ConnectorEvent::DocumentCreated {
                document_id: SourceDocumentId::new(file_key.clone()),
                occurred_at,
            });
            if !file.version.is_empty() {
                versions.insert(file_key.clone(), file.version.clone());
            }
            for comp in resp.components {
                let comp_when = comp.created_at.or(comp.updated_at).unwrap_or_else(Utc::now);
                events.push(ConnectorEvent::DocumentCreated {
                    document_id: SourceDocumentId::new(format!("component:{}", comp.key)),
                    occurred_at: comp_when,
                });
            }
        }
        Ok(SyncRunResult {
            events,
            next_cursor: encode_cursor(&versions),
        })
    }

    fn incremental_sync(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        state: &SyncState,
    ) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        let file_keys = Self::configured_file_keys(config)?;
        let prior = decode_cursor(state.cursor.as_deref());
        let mut events: Vec<ConnectorEvent> = Vec::new();
        let mut next = prior.clone();
        for file_key in &file_keys {
            let resp = self.fetch_file(&base_url, token, file_key)?;
            let file = resp.file.unwrap_or_default();
            // Skip if the file version is at or before the prior
            // watermark — Figma versions are monotonic.
            if let Some(prev) = prior.get(file_key) {
                if !file.version.is_empty() && version_at_or_before(&file.version, prev) {
                    continue;
                }
            }
            let occurred_at = file.last_modified.unwrap_or_else(Utc::now);
            events.push(ConnectorEvent::DocumentUpdated {
                document_id: SourceDocumentId::new(file_key.clone()),
                occurred_at,
            });
            if !file.version.is_empty() {
                next.insert(file_key.clone(), file.version.clone());
            }
            for comp in resp.components {
                let comp_when = comp.updated_at.or(comp.created_at).unwrap_or_else(Utc::now);
                events.push(ConnectorEvent::DocumentUpdated {
                    document_id: SourceDocumentId::new(format!("component:{}", comp.key)),
                    occurred_at: comp_when,
                });
            }
        }
        Ok(SyncRunResult {
            events,
            next_cursor: encode_cursor(&next).or_else(|| state.cursor.clone()),
        })
    }

    fn fetch_content(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        document_id: &SourceDocumentId,
    ) -> Result<FetchedContent> {
        let base_url = self.resolved_base_url(config);
        let raw_id = document_id.as_str();
        // The substrate also mints `component:<key>` ids, which are
        // not addressable as standalone files — only file documents
        // carry a fetchable body.
        if raw_id.starts_with("component:") {
            return Err(ConnectorError::Sync(format!(
                "figma fetch_content: component document {raw_id} has no standalone body; fetch \
                 its containing file instead"
            )));
        }
        let file_key = raw_id;
        let key_enc = percent_encode_path_component(file_key);

        // 1. Full file tree → extract text + renderable node ids.
        let file_url = format!("{base_url}/v1/files/{key_enc}");
        let file: serde_json::Value = bearer_get_json(
            &self.transport,
            "figma",
            "/v1/files/{key}",
            &file_url,
            token,
            &[],
        )?;
        let name = file
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        let mut text_nodes: Vec<String> = Vec::new();
        let mut render_ids: Vec<String> = Vec::new();
        if let Some(document) = file.get("document") {
            collect_text_nodes(document, 0, &mut text_nodes);
            collect_render_node_ids(document, 0, &mut render_ids);
        }
        let body = text_nodes.join("\n");

        // 2. Rendered PNGs for the collected frame nodes (best-effort:
        //    Figma returns a node id → URL map; null entries are
        //    dropped). Skipped entirely when there are no frames.
        let mut images: BTreeMap<String, String> = BTreeMap::new();
        if !render_ids.is_empty() {
            // Encode each node id individually, then join with literal
            // commas so Figma sees a comma-delimited `ids=` list.
            let ids_param = render_ids
                .iter()
                .map(|id| percent_encode_path_component(id))
                .collect::<Vec<_>>()
                .join(",");
            let images_url = format!("{base_url}/v1/images/{key_enc}?ids={ids_param}&format=png");
            let resp: FigmaImagesResponse = bearer_get_json(
                &self.transport,
                "figma",
                "/v1/images/{key}",
                &images_url,
                token,
                &[],
            )?;
            if let Some(err) = resp.err.filter(|e| !e.is_empty()) {
                return Err(ConnectorError::Sync(format!(
                    "figma /v1/images/{file_key} returned err={err}"
                )));
            }
            for (node_id, url) in resp.images {
                if let Some(url) = url {
                    images.insert(node_id, url);
                }
            }
        }

        let source_url = format!("https://www.figma.com/file/{file_key}");
        Ok(FetchedContent::text(body, "text/plain")
            .with_title(name)
            .with_metadata(serde_json::json!({
                "provider": "figma",
                "file_key": file_key,
                "text_node_count": text_nodes.len(),
                "rendered_images": images,
            }))
            .with_source_url(source_url))
    }

    fn subscribe_webhook(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        callback_url: &str,
    ) -> Result<WebhookSubscription> {
        // Figma's `/v2/webhooks` endpoint registers ONE event_type
        // per call — we batch all five into separate requests and
        // capture the first id in `provider_subscription_id`. The
        // remainder ids are appended to the `metadata.notes` field
        // (a free-form area) so the operator can audit them.
        const FIGMA_EVENT_TYPES: &[&str] = &[
            "FILE_UPDATE",
            "FILE_VERSION_UPDATE",
            "FILE_DELETE",
            "LIBRARY_PUBLISH",
            "FILE_PERMISSION_UPDATE",
        ];
        let base_url = self.resolved_base_url(config);
        let team_ids = Self::configured_team_ids(config)?;
        let passcode = config
            .auth_config_json
            .get("webhook_secret")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("figma-passcode-secret")
            .to_string();
        let mut registered_ids: Vec<String> = Vec::new();
        for team_id in &team_ids {
            for event_type in FIGMA_EVENT_TYPES {
                let url = format!("{base_url}/v2/webhooks");
                let body = serde_json::json!({
                    "event_type": event_type,
                    "team_id": team_id,
                    "endpoint": callback_url,
                    "passcode": passcode,
                });
                let resp: Result<FigmaWebhookCreateResponse> = bearer_post_json(
                    &self.transport,
                    "figma",
                    "/v2/webhooks",
                    &url,
                    token,
                    &[],
                    &body,
                );
                match resp {
                    Ok(payload) => {
                        if let Some(id) = payload.id {
                            registered_ids.push(id);
                        } else {
                            // Partial-registration rollback: Figma
                            // accepted the call but didn't return
                            // an id, so we have no way to revoke
                            // it later. Clean up everything we did
                            // get an id for so the operator's
                            // dashboard isn't littered with orphan
                            // webhooks.
                            self.rollback_partial_webhooks(&base_url, token, &registered_ids);
                            return Err(ConnectorError::Webhook(
                                "figma /v2/webhooks returned no id on registration".into(),
                            ));
                        }
                    }
                    Err(e) => {
                        self.rollback_partial_webhooks(&base_url, token, &registered_ids);
                        return Err(e);
                    }
                }
            }
        }
        if registered_ids.is_empty() {
            // No event_types were registered — typically because
            // `auth_config_json.team_ids` resolved to an empty
            // list. Surface this as a webhook error so the
            // substrate doesn't persist a half-formed subscription
            // with an empty `provider_subscription_id`.
            return Err(ConnectorError::Webhook(
                "figma subscribe_webhook: no event_types were registered (empty team_ids?)".into(),
            ));
        }
        let mut subscription = WebhookSubscription::new(
            self.instance,
            callback_url,
            WebhookSecret::new(passcode),
            WebhookEventTypes::all(),
            // Figma webhooks have no provider TTL.
            None,
        );
        // All ids are stored in a comma-joined list; the revoke
        // path splits on `,` and issues `DELETE /v2/webhooks/{id}`
        // per element.
        subscription.provider_subscription_id = Some(registered_ids.join(","));
        Ok(subscription)
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
    use chrono::Duration;
    use connector_framework::{
        AuthKind, ConnectorKind, HttpMethod, MockHttpTransport, MockResponse,
    };
    use evidence_store::ScopeId;

    struct FixedOAuth;
    impl OAuth2CodeExchange for FixedOAuth {
        fn exchange_code(&self, _config: &ConnectorConfig, _code: &str) -> Result<OAuth2Token> {
            Ok(OAuth2Token::new(
                "figma-access",
                "figma-refresh",
                Utc::now() + Duration::hours(24),
                "files:read file_metadata:read library_assets:read webhooks:write",
            ))
        }
    }

    fn oauth() -> Arc<dyn OAuth2CodeExchange> {
        Arc::new(FixedOAuth)
    }

    fn cfg_with(extra: &serde_json::Value) -> ConnectorConfig {
        let mut base = serde_json::json!({
            "authorization_code": "demo-code",
            "api_base_url": "https://api.test/figma",
            "file_keys": ["F1"],
            "team_ids": ["team-1"],
            "webhook_secret": "demo-passcode",
        });
        let base_map = base.as_object_mut().unwrap();
        if let Some(extra_map) = extra.as_object() {
            for (k, v) in extra_map {
                base_map.insert(k.clone(), v.clone());
            }
        }
        ConnectorConfig::new(ConnectorKind::Figma, AuthKind::OAuth2, ScopeId::new_v4())
            .with_auth_config(base)
    }

    fn cfg() -> ConnectorConfig {
        cfg_with(&serde_json::Value::Object(serde_json::Map::new()))
    }

    fn ok_json(value: &serde_json::Value) -> MockResponse {
        MockResponse::ok_json(serde_json::to_vec(value).unwrap())
    }

    /// Figma's wire format: file metadata at the root, components as
    /// a JSON map keyed by node id.
    fn wire_file(version: &str) -> serde_json::Value {
        serde_json::json!({
            "name": "Design",
            "version": version,
            "lastModified": Utc::now(),
            "thumbnailUrl": "https://figma.test/thumb",
            "components": {
                "1:1": {
                    "key": "comp-1",
                    "name": "Button",
                    "description": "Primary",
                    "created_at": Utc::now(),
                    "updated_at": Utc::now(),
                }
            }
        })
    }

    #[test]
    fn authenticate_dispatches_to_oauth_exchange() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = FigmaConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        assert!(tok.scope.contains("files:read"));
    }

    #[test]
    fn authenticate_requires_authorization_code() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = FigmaConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let cfg_no_code =
            ConnectorConfig::new(ConnectorKind::Figma, AuthKind::OAuth2, ScopeId::new_v4());
        let err = c.authenticate(&cfg_no_code).unwrap_err();
        assert!(matches!(err, ConnectorError::Auth(_)));
    }

    #[test]
    fn initial_sync_emits_per_file_and_per_component() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/figma/v1/files/F1",
            ok_json(&wire_file("100")),
        );
        let c = FigmaConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 2);
        let cursor = decode_cursor(res.next_cursor.as_deref());
        assert_eq!(cursor.get("F1").map(String::as_str), Some("100"));
    }

    #[test]
    fn initial_sync_requires_configured_file_keys() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = FigmaConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let cfg_no_keys =
            ConnectorConfig::new(ConnectorKind::Figma, AuthKind::OAuth2, ScopeId::new_v4())
                .with_auth_config(serde_json::json!({
                    "authorization_code": "demo-code",
                    "api_base_url": "https://api.test/figma",
                }));
        let tok = c.authenticate(&cfg()).unwrap();
        let err = c.initial_sync(&cfg_no_keys, &tok).unwrap_err();
        assert!(matches!(err, ConnectorError::Sync(_)));
    }

    #[test]
    fn incremental_sync_skips_unchanged_files_via_per_file_cursor() {
        // Cursor pins F1 at version 100. The transport returns
        // version 100 again — incremental_sync must emit nothing.
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/figma/v1/files/F1",
            ok_json(&wire_file("100")),
        );
        let c = FigmaConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let mut state = SyncState::new(c.instance);
        state.cursor =
            Some(encode_cursor(&BTreeMap::from([("F1".to_string(), "100".to_string())])).unwrap());
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        assert!(res.events.is_empty());
        // Cursor must round-trip — same versions retained.
        assert_eq!(
            decode_cursor(res.next_cursor.as_deref())
                .get("F1")
                .map(String::as_str),
            Some("100"),
        );
    }

    #[test]
    fn incremental_sync_compares_versions_numerically() {
        // Cursor at 9 → version 10 must register as newer (regression
        // against lexicographic compare that would treat "10" <= "9").
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/figma/v1/files/F1",
            ok_json(&wire_file("10")),
        );
        let c = FigmaConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let mut state = SyncState::new(c.instance);
        state.cursor =
            Some(encode_cursor(&BTreeMap::from([("F1".to_string(), "9".to_string())])).unwrap());
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        // File + 1 component → 2 events.
        assert_eq!(res.events.len(), 2);
        assert!(matches!(
            res.events[0],
            ConnectorEvent::DocumentUpdated { .. }
        ));
        let next = decode_cursor(res.next_cursor.as_deref());
        assert_eq!(next.get("F1").map(String::as_str), Some("10"));
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
    fn subscribe_webhook_posts_once_per_event_type() {
        // We register 5 event types × 1 team = 5 POSTs. The mock
        // returns a distinct id per event_type so we can verify the
        // joined id list reaches `provider_subscription_id`.
        let transport = Arc::new(MockHttpTransport::new());
        for id in ["w1", "w2", "w3", "w4", "w5"] {
            transport.expect(
                HttpMethod::Post,
                "https://api.test/figma/v2/webhooks",
                ok_json(&serde_json::json!({"id": id})),
            );
        }
        let c = FigmaConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let sub = c
            .subscribe_webhook(&cfg(), &tok, "https://demo.example/webhooks/figma")
            .unwrap();
        assert_eq!(
            sub.provider_subscription_id.as_deref(),
            Some("w1,w2,w3,w4,w5"),
        );
        // Every recorded request must carry the configured passcode
        // so Figma can echo it back for signature verification.
        let recorded = transport.recorded();
        for req in &recorded {
            let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            assert_eq!(body["passcode"], "demo-passcode");
            assert_eq!(body["team_id"], "team-1");
            assert_eq!(body["endpoint"], "https://demo.example/webhooks/figma");
        }
    }

    #[test]
    fn subscribe_webhook_rolls_back_successful_registrations_on_failure() {
        // Setup: the connector registers 5 event_types per team.
        // Mock first two POSTs success (`w1`, `w2`), third POST
        // returns 500 (transport retries exhaust → ConnectorError).
        // After the failure, the connector must issue DELETE
        // requests for `w1` and `w2` before returning Err — so no
        // orphan webhooks linger in Figma's dashboard.
        let transport = Arc::new(MockHttpTransport::new());
        // FIFO success POSTs:
        transport.expect(
            HttpMethod::Post,
            "https://api.test/figma/v2/webhooks",
            ok_json(&serde_json::json!({"id": "w1"})),
        );
        transport.expect(
            HttpMethod::Post,
            "https://api.test/figma/v2/webhooks",
            ok_json(&serde_json::json!({"id": "w2"})),
        );
        // Third POST returns a hard 4xx (no transport retry; the
        // failure propagates as ConnectorError::Sync or Webhook).
        transport.expect(
            HttpMethod::Post,
            "https://api.test/figma/v2/webhooks",
            MockResponse::status(403, b"forbidden".to_vec()),
        );
        // Expected rollback DELETEs — these are the assertions we
        // care about for this test.
        transport.expect(
            HttpMethod::Delete,
            "https://api.test/figma/v2/webhooks/w1",
            MockResponse::status(204, Vec::new()),
        );
        transport.expect(
            HttpMethod::Delete,
            "https://api.test/figma/v2/webhooks/w2",
            MockResponse::status(204, Vec::new()),
        );

        let c = FigmaConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let err = c
            .subscribe_webhook(&cfg(), &tok, "https://demo.example/webhooks/figma")
            .unwrap_err();
        assert!(
            matches!(
                err,
                ConnectorError::Sync(_) | ConnectorError::Webhook(_) | ConnectorError::Auth(_)
            ),
            "subscribe_webhook must surface the upstream failure"
        );
        // Verify that both rollback DELETEs were actually issued:
        let deletes: Vec<_> = transport
            .recorded()
            .into_iter()
            .filter(|r| r.method == HttpMethod::Delete)
            .collect();
        assert_eq!(
            deletes.len(),
            2,
            "must issue one DELETE per successfully-registered webhook id"
        );
        assert_eq!(deletes[0].url, "https://api.test/figma/v2/webhooks/w1");
        assert_eq!(deletes[1].url, "https://api.test/figma/v2/webhooks/w2");
    }

    #[test]
    fn subscribe_webhook_requires_team_ids() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = FigmaConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let cfg_no_teams =
            ConnectorConfig::new(ConnectorKind::Figma, AuthKind::OAuth2, ScopeId::new_v4())
                .with_auth_config(serde_json::json!({
                    "authorization_code": "demo-code",
                    "api_base_url": "https://api.test/figma",
                    "file_keys": ["F1"],
                }));
        let tok = c.authenticate(&cfg()).unwrap();
        let err = c
            .subscribe_webhook(&cfg_no_teams, &tok, "https://demo.example/webhooks/figma")
            .unwrap_err();
        assert!(matches!(err, ConnectorError::Webhook(_)));
    }

    #[test]
    fn list_500_propagates_as_sync_error() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/figma/v1/files/F1",
            MockResponse::status(500, b"upstream boom".to_vec()),
        );
        let c = FigmaConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let err = c.initial_sync(&cfg(), &tok).unwrap_err();
        assert!(matches!(err, ConnectorError::Sync(_)));
    }

    #[test]
    fn webhook_parses_file_delete() {
        let body = serde_json::json!({
            "event_type": "FILE_DELETE",
            "file_key": "F2",
            "timestamp": Utc::now(),
        });
        let transport = Arc::new(MockHttpTransport::new());
        let c = FigmaConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
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
        let transport = Arc::new(MockHttpTransport::new());
        let c = FigmaConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
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
        let transport = Arc::new(MockHttpTransport::new());
        let c = FigmaConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let err = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap_err();
        assert!(matches!(err, ConnectorError::Webhook(_)));
    }

    // ───────────── fetch_content ─────────────

    #[test]
    fn fetch_content_extracts_text_and_rendered_images() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/figma/v1/files/F1",
            ok_json(&serde_json::json!({
                "name": "Onboarding",
                "document": {
                    "id": "0:0", "type": "DOCUMENT",
                    "children": [{
                        "id": "0:1", "type": "CANVAS", "name": "Page 1",
                        "children": [{
                            "id": "1:2", "type": "FRAME", "name": "Welcome",
                            "children": [
                                { "id": "1:3", "type": "TEXT", "characters": "Welcome aboard" },
                                { "id": "1:4", "type": "TEXT", "characters": "Get started" }
                            ]
                        }]
                    }]
                }
            })),
        );
        // Node id "1:2" → "1%3A2" once percent-encoded.
        transport.expect(
            HttpMethod::Get,
            "https://api.test/figma/v1/images/F1?ids=1%3A2&format=png",
            ok_json(&serde_json::json!({
                "err": serde_json::Value::Null,
                "images": { "1:2": "https://figma-cdn.test/render/1-2.png" }
            })),
        );
        let c = FigmaConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let fc = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("F1"))
            .unwrap();
        let body = String::from_utf8(fc.body).unwrap();
        assert_eq!(body, "Welcome aboard\nGet started");
        assert_eq!(fc.mime_type, "text/plain");
        assert_eq!(fc.title.as_deref(), Some("Onboarding"));
        assert_eq!(
            fc.source_url.as_deref(),
            Some("https://www.figma.com/file/F1")
        );
        assert_eq!(fc.metadata["text_node_count"], serde_json::json!(2));
        assert_eq!(
            fc.metadata["rendered_images"]["1:2"],
            serde_json::json!("https://figma-cdn.test/render/1-2.png")
        );
    }

    #[test]
    fn fetch_content_handles_file_without_frames() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/figma/v1/files/F2",
            ok_json(&serde_json::json!({
                "name": "Empty",
                "document": { "id": "0:0", "type": "DOCUMENT", "children": [] }
            })),
        );
        let c = FigmaConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let fc = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("F2"))
            .unwrap();
        // No frames → no /v1/images call; body empty, no images.
        assert!(String::from_utf8(fc.body).unwrap().is_empty());
        assert_eq!(fc.metadata["text_node_count"], serde_json::json!(0));
        assert_eq!(fc.metadata["rendered_images"], serde_json::json!({}));
    }

    #[test]
    fn fetch_content_rejects_component_documents() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = FigmaConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let err = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("component:abc"))
            .unwrap_err();
        assert!(matches!(err, ConnectorError::Sync(_)));
    }

    #[test]
    fn fetch_content_maps_404_to_sync_error() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/figma/v1/files/F404",
            MockResponse::status(404, br#"{"err":"Not found"}"#.to_vec()),
        );
        let c = FigmaConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let err = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("F404"))
            .unwrap_err();
        assert!(matches!(err, ConnectorError::Sync(_)));
    }
}
