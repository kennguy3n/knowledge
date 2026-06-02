//! HubSpot connector — HubSpot CRM v3 API.
//!
//! * `initial_sync` walks `/crm/v3/objects/{type}?limit=100&after=…`
//!   for every configured object kind (`contacts` is the default;
//!   `auth_config_json.object_kinds` lets tenants opt in to
//!   companies / deals / notes / line items). Pagination follows
//!   HubSpot's `paging.next.after` token until the server stops
//!   returning a next cursor.
//! * `incremental_sync` POSTs `/crm/v3/objects/{type}/search` with a
//!   `lastmodifieddate >= <watermark>` filter, sorted ascending so
//!   the substrate can advance the watermark in order.
//! * `subscribe_webhook` POSTs `/webhooks/v3/{appId}/subscriptions`
//!   once per `(objectType × subscriptionType)` pair (HubSpot only
//!   accepts one event per subscription create). Every assigned
//!   numeric id is concatenated into
//!   [`WebhookSubscription::provider_subscription_id`] for later
//!   revocation / re-registration.
//! * `handle_webhook_event` parses HubSpot's batched JSON-array
//!   webhook payload — every event in the batch is surfaced,
//!   unknown subscription types are skipped (not errored) so a new
//!   event family can't discard valid events queued behind it.
//!
//! Production wiring runs over [`HttpTransport`] — the substrate
//! constructs a [`HubSpotConnector`] with a real
//! `connector_framework::BlockingHttpTransport` (under the
//! `http-client` feature) and a real `OAuth2Client` for the
//! `https://api.hubapi.com/oauth/v1/token` exchange. Unit tests
//! pass `MockHttpTransport` + a fixture OAuth2 exchange.

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

use crate::content::strip_html;

/// Default HubSpot REST base URL. Per-instance overrides go through
/// `auth_config_json.api_base_url`.
pub const DEFAULT_API_BASE_URL: &str = "https://api.hubapi.com";

/// Page size for `/crm/v3/objects/{type}` and the matching `/search`
/// endpoint. HubSpot's documented max is 100.
pub const DEFAULT_PAGE_SIZE: u32 = 100;

/// Safety ceiling on number of pages a single sync will walk —
/// catches mis-shaped `paging.next.after` cursors that lie about
/// end-of-list.
pub const MAX_LIST_PAGES: usize = 10_000;

/// Upper bound on property names requested when rendering a single CRM
/// object. Real object types expose well under this many properties;
/// the cap keeps the `properties=` query string bounded for portals
/// with a pathologically large custom-property catalogue.
const MAX_FETCH_PROPERTIES: usize = 500;

/// HubSpot CRM object kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HubSpotObjectKind {
    /// `contacts`
    Contact,
    /// `companies`
    Company,
    /// `deals`
    Deal,
    /// `notes`
    Note,
}

impl HubSpotObjectKind {
    /// Path-segment form for `/crm/v3/objects/{type}` (HubSpot's URL
    /// uses the plural for some kinds — keep this in sync with
    /// upstream).
    #[must_use]
    pub fn as_path_segment(self) -> &'static str {
        match self {
            HubSpotObjectKind::Contact => "contacts",
            HubSpotObjectKind::Company => "companies",
            HubSpotObjectKind::Deal => "deals",
            HubSpotObjectKind::Note => "notes",
        }
    }

    /// Parse from the `auth_config_json.object_kinds` array.
    #[must_use]
    pub fn from_config_str(s: &str) -> Option<Self> {
        match s {
            "contact" | "contacts" => Some(Self::Contact),
            "company" | "companies" => Some(Self::Company),
            "deal" | "deals" => Some(Self::Deal),
            "note" | "notes" => Some(Self::Note),
            _ => None,
        }
    }
}

/// One CRM object (subset).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HubSpotObject {
    /// Object id.
    pub id: String,
    /// Object kind. Defaults to [`HubSpotObjectKind::Contact`] when
    /// not echoed in the wire payload — the per-kind list endpoint
    /// already disambiguates by URL.
    #[serde(default = "default_object_kind")]
    pub kind: HubSpotObjectKind,
    /// `createdAt` timestamp.
    #[serde(default, rename = "createdAt")]
    pub created_at: Option<DateTime<Utc>>,
    /// `updatedAt` timestamp.
    #[serde(default, rename = "updatedAt")]
    pub updated_at: Option<DateTime<Utc>>,
    /// `archived = true` is the deletion signal.
    #[serde(default)]
    pub archived: bool,
}

fn default_object_kind() -> HubSpotObjectKind {
    HubSpotObjectKind::Contact
}

/// Full single-object response from `GET /crm/v3/objects/{type}/{id}`
/// — carries the requested `properties` map plus any requested
/// `associations`.
#[derive(Debug, Clone, Default, Deserialize)]
struct HubSpotObjectDetail {
    #[serde(default)]
    properties: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    associations: BTreeMap<String, HubSpotAssociationGroup>,
}

/// One `associations.{type}` group on an object detail response.
#[derive(Debug, Clone, Default, Deserialize)]
struct HubSpotAssociationGroup {
    #[serde(default)]
    results: Vec<HubSpotAssociationRef>,
}

/// `GET /crm/v3/properties/{objectType}` response — the catalogue of
/// property definitions for an object type. Used to enumerate every
/// property name to request on a single-object fetch (the CRM v3 object
/// endpoint has no `*` wildcard; the `properties` query param is a
/// comma-separated allow-list and unknown names are silently ignored).
#[derive(Debug, Clone, Default, Deserialize)]
struct HubSpotPropertyList {
    #[serde(default)]
    results: Vec<HubSpotPropertyDef>,
}

/// One property definition (only the `name` is needed).
#[derive(Debug, Clone, Default, Deserialize)]
struct HubSpotPropertyDef {
    #[serde(default)]
    name: String,
}

/// One associated object reference.
#[derive(Debug, Clone, Default, Deserialize)]
struct HubSpotAssociationRef {
    #[serde(default)]
    id: String,
}

/// Render a HubSpot property value (string / number / bool) into a
/// display string. Returns an empty string for null / nested values.
fn property_value_to_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        _ => String::new(),
    }
}

/// Derive a human-readable title for a CRM object from its property
/// map, using the kind-appropriate naming property.
fn hubspot_title(
    kind: HubSpotObjectKind,
    properties: &BTreeMap<String, serde_json::Value>,
) -> Option<String> {
    let get = |key: &str| {
        properties
            .get(key)
            .map(property_value_to_string)
            .filter(|s| !s.is_empty())
    };
    match kind {
        HubSpotObjectKind::Contact => {
            let parts: Vec<String> = ["firstname", "lastname"]
                .iter()
                .filter_map(|k| get(k))
                .collect();
            if parts.is_empty() {
                get("email")
            } else {
                Some(parts.join(" "))
            }
        }
        HubSpotObjectKind::Company => get("name"),
        HubSpotObjectKind::Deal => get("dealname"),
        HubSpotObjectKind::Note => None,
    }
}

/// One page of `/crm/v3/objects/{type}` results.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HubSpotListResponse {
    /// Object records on this page.
    #[serde(default)]
    pub results: Vec<HubSpotObject>,
    /// Paging cursor — `paging.next.after`.
    #[serde(default)]
    pub paging: Option<HubSpotPaging>,
}

/// HubSpot paging envelope.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HubSpotPaging {
    /// `next` block.
    #[serde(default)]
    pub next: Option<HubSpotPagingNext>,
}

/// HubSpot `paging.next` cursor.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HubSpotPagingNext {
    /// `after` opaque cursor token.
    pub after: String,
}

/// Response from `POST /webhooks/v3/{appId}/subscriptions`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HubSpotSubscriptionCreateResponse {
    /// Numeric subscription id HubSpot assigned.
    #[serde(default)]
    pub id: Option<i64>,
    /// Whether the subscription is active (HubSpot returns
    /// `active: false` if the app's webhook target isn't verified).
    #[serde(default)]
    pub active: Option<bool>,
}

/// One HubSpot webhook event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HubSpotWebhookEvent {
    /// Subscription event type:
    /// `contact.creation`, `contact.propertyChange`, `contact.deletion`,
    /// `company.creation`, …
    #[serde(rename = "subscriptionType")]
    pub subscription_type: String,
    /// `objectId` (HubSpot's int id, serialised as a number).
    #[serde(rename = "objectId")]
    pub object_id: i64,
    /// `occurredAt` (millis since epoch).
    #[serde(default, rename = "occurredAt")]
    pub occurred_at_ms: Option<i64>,
    /// `propertyName` (only on propertyChange).
    #[serde(default, rename = "propertyName")]
    pub property_name: Option<String>,
    /// `propertyValue` (only on propertyChange).
    #[serde(default, rename = "propertyValue")]
    pub property_value: Option<String>,
    /// `userId` whose permission changed.
    #[serde(default, rename = "userId")]
    pub user_id: Option<String>,
}

/// HubSpot connector.
///
/// Per-tenant `object_kinds`, `app_id`, and `webhook_secret` are
/// read from `auth_config_json` on every call; the substrate
/// persists them at install time.
#[derive(Clone)]
pub struct HubSpotConnector {
    /// Connector instance id.
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    oauth: Arc<dyn OAuth2CodeExchange>,
    api_base_url: String,
    page_size: u32,
}

impl std::fmt::Debug for HubSpotConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HubSpotConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("page_size", &self.page_size)
            .field("transport", &"<HttpTransport>")
            .field("oauth", &"<OAuth2CodeExchange>")
            .finish()
    }
}

impl HubSpotConnector {
    /// Construct a HubSpot connector.
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
            page_size: DEFAULT_PAGE_SIZE,
        }
    }

    /// Override the HubSpot REST base URL.
    #[must_use]
    pub fn with_api_base_url(mut self, url: impl Into<String>) -> Self {
        self.api_base_url = url.into();
        self
    }

    /// Override the page size used by the list/search endpoints.
    /// Clamped to `[1, 100]` per HubSpot's documented maximum.
    #[must_use]
    pub fn with_page_size(mut self, size: u32) -> Self {
        self.page_size = size.clamp(1, 100);
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

    /// Enumerate every property name defined for an object type via
    /// `GET /crm/v3/properties/{objectType}`.
    ///
    /// The CRM v3 single-object endpoint has no `*` wildcard — its
    /// `properties` query param is a comma-separated allow-list and any
    /// name it doesn't recognise is silently dropped, so a literal `*`
    /// would return only the default properties. Listing the catalogue
    /// first lets the object fetch request the full set. Names are
    /// de-duplicated, sorted for a deterministic query string, and
    /// capped at [`MAX_FETCH_PROPERTIES`].
    fn fetch_object_property_names(
        &self,
        base_url: &str,
        token: &OAuth2Token,
        type_seg: &str,
    ) -> Result<Vec<String>> {
        let url = format!("{base_url}/crm/v3/properties/{type_seg}");
        let list: HubSpotPropertyList = bearer_get_json(
            &self.transport,
            "hubspot",
            "/crm/v3/properties/{type}",
            &url,
            token,
            &[],
        )?;
        let mut names: Vec<String> = list
            .results
            .into_iter()
            .map(|p| p.name)
            .filter(|n| !n.is_empty())
            .collect();
        names.sort();
        names.dedup();
        names.truncate(MAX_FETCH_PROPERTIES);
        Ok(names)
    }

    /// Resolve the configured object kinds. Defaults to
    /// `[Contact]` when the tenant didn't customise the list.
    fn configured_kinds(config: &ConnectorConfig) -> Result<Vec<HubSpotObjectKind>> {
        let Some(raw) = config
            .auth_config_json
            .get("object_kinds")
            .and_then(serde_json::Value::as_array)
        else {
            return Ok(vec![HubSpotObjectKind::Contact]);
        };
        let mut kinds = Vec::<HubSpotObjectKind>::new();
        for v in raw {
            if let Some(s) = v.as_str() {
                if let Some(k) = HubSpotObjectKind::from_config_str(s) {
                    if !kinds.contains(&k) {
                        kinds.push(k);
                    }
                } else {
                    return Err(ConnectorError::Sync(format!(
                        "hubspot: auth_config_json.object_kinds[{s}] is not a known kind"
                    )));
                }
            }
        }
        if kinds.is_empty() {
            return Err(ConnectorError::Sync(
                "hubspot: auth_config_json.object_kinds was present but contained no kinds".into(),
            ));
        }
        Ok(kinds)
    }

    /// Resolve the app id used to scope webhook subscription POSTs.
    fn configured_app_id(config: &ConnectorConfig) -> Result<String> {
        config
            .auth_config_json
            .get("app_id")
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.is_empty())
            .map(std::string::ToString::to_string)
            .ok_or_else(|| {
                ConnectorError::Webhook(
                    "hubspot subscribe_webhook: auth_config_json.app_id is required".into(),
                )
            })
    }

    /// Best-effort cleanup of subscription POSTs that succeeded
    /// before a later POST in `subscribe_webhook` failed.
    ///
    /// HubSpot's `POST /webhooks/v3/{appId}/subscriptions` registers
    /// **one** event type per call (the API rejects multi-event
    /// bodies), so a single `subscribe_webhook` invocation iterates
    /// the `(objectType × subscriptionType)` matrix and issues N
    /// independent POSTs. If any POST after the first one fails, the
    /// caller has decided to fail the whole `subscribe_webhook` —
    /// which means the registered ids will never be persisted via
    /// [`WebhookSubscription::provider_subscription_id`]. Without
    /// this rollback the orphaned subscriptions would keep firing
    /// webhooks the substrate can't correlate.
    ///
    /// Cleanup is best-effort: each `DELETE
    /// /webhooks/v3/{appId}/subscriptions/{id}` is attempted and
    /// failures are swallowed (we already have an outer error to
    /// surface; a secondary failure shouldn't mask it). The original
    /// error is always preserved by the caller via `return Err(_)`
    /// after this method returns. Mirrors the equivalent helper in
    /// `figma.rs::rollback_partial_webhooks`.
    fn rollback_partial_webhooks(
        &self,
        base_url: &str,
        app_id: &str,
        token: &OAuth2Token,
        ids: &[String],
    ) {
        for id in ids {
            let url = format!("{base_url}/webhooks/v3/{app_id}/subscriptions/{id}");
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

    fn paginate_list(
        &self,
        base_url: &str,
        token: &OAuth2Token,
        kind: HubSpotObjectKind,
    ) -> Result<Vec<HubSpotObject>> {
        let mut objects = Vec::<HubSpotObject>::new();
        let mut after: Option<String> = None;
        for _ in 0..MAX_LIST_PAGES {
            let url = match after.as_deref() {
                Some(cursor) => format!(
                    "{base_url}/crm/v3/objects/{}?limit={}&after={}",
                    kind.as_path_segment(),
                    self.page_size,
                    connector_framework::percent_encode_path_component(cursor),
                ),
                None => format!(
                    "{base_url}/crm/v3/objects/{}?limit={}",
                    kind.as_path_segment(),
                    self.page_size,
                ),
            };
            let resp: HubSpotListResponse = bearer_get_json(
                &self.transport,
                "hubspot",
                "/crm/v3/objects",
                &url,
                token,
                &[],
            )?;
            objects.extend(resp.results.into_iter().map(|mut o| {
                o.kind = kind;
                o
            }));
            let Some(next) = resp.paging.and_then(|p| p.next) else {
                return Ok(objects);
            };
            if next.after.is_empty() {
                return Ok(objects);
            }
            // Loop guard — protect against pathological servers that
            // echo the same cursor twice.
            if after.as_deref() == Some(next.after.as_str()) {
                return Ok(objects);
            }
            after = Some(next.after);
        }
        Err(ConnectorError::Sync(format!(
            "hubspot /crm/v3/objects/{} exceeded {MAX_LIST_PAGES} pages without exhausting cursor",
            kind.as_path_segment()
        )))
    }

    fn paginate_search(
        &self,
        base_url: &str,
        token: &OAuth2Token,
        kind: HubSpotObjectKind,
        cursor_ms: i64,
    ) -> Result<Vec<HubSpotObject>> {
        let url = format!(
            "{base_url}/crm/v3/objects/{}/search",
            kind.as_path_segment()
        );
        let mut objects = Vec::<HubSpotObject>::new();
        let mut after: Option<String> = None;
        for _ in 0..MAX_LIST_PAGES {
            let mut body = serde_json::json!({
                "filterGroups": [{
                    "filters": [{
                        "propertyName": "hs_lastmodifieddate",
                        "operator": "GTE",
                        "value": cursor_ms,
                    }]
                }],
                "sorts": [{
                    "propertyName": "hs_lastmodifieddate",
                    "direction": "ASCENDING"
                }],
                "limit": self.page_size,
            });
            if let Some(cursor) = after.as_deref() {
                body.as_object_mut().unwrap().insert(
                    "after".to_string(),
                    serde_json::Value::String(cursor.to_string()),
                );
            }
            let resp: HubSpotListResponse = bearer_post_json(
                &self.transport,
                "hubspot",
                "/crm/v3/objects/search",
                &url,
                token,
                &[],
                &body,
            )?;
            objects.extend(resp.results.into_iter().map(|mut o| {
                o.kind = kind;
                o
            }));
            let Some(next) = resp.paging.and_then(|p| p.next) else {
                return Ok(objects);
            };
            if next.after.is_empty() {
                return Ok(objects);
            }
            if after.as_deref() == Some(next.after.as_str()) {
                return Ok(objects);
            }
            after = Some(next.after);
        }
        Err(ConnectorError::Sync(format!("hubspot /crm/v3/objects/{}/search exceeded {MAX_LIST_PAGES} pages without exhausting cursor",
            kind.as_path_segment()
        )))
    }
}

/// Which sync pass produced this object — we use this instead of
/// comparing `created_at == updated_at` because HubSpot may set
/// the two timestamps to slightly different millisecond instants
/// even on creation, which would silently misclassify the event.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum SyncMode {
    Initial,
    Incremental,
}

fn object_to_event(obj: &HubSpotObject, mode: SyncMode) -> ConnectorEvent {
    let occurred_at = obj.updated_at.or(obj.created_at).unwrap_or_else(Utc::now);
    let id = SourceDocumentId::new(format!("{}:{}", kind_str(obj.kind), obj.id));
    if obj.archived {
        return ConnectorEvent::DocumentDeleted {
            document_id: id,
            occurred_at,
        };
    }
    match mode {
        SyncMode::Initial => ConnectorEvent::DocumentCreated {
            document_id: id,
            occurred_at,
        },
        SyncMode::Incremental => ConnectorEvent::DocumentUpdated {
            document_id: id,
            occurred_at,
        },
    }
}

fn kind_str(k: HubSpotObjectKind) -> &'static str {
    match k {
        HubSpotObjectKind::Contact => "contact",
        HubSpotObjectKind::Company => "company",
        HubSpotObjectKind::Deal => "deal",
        HubSpotObjectKind::Note => "note",
    }
}

fn parse_role(role: &str) -> Option<SourcePermissionLevel> {
    match role {
        "viewer" | "view" | "read" => Some(SourcePermissionLevel::Read),
        "editor" | "edit" | "write" => Some(SourcePermissionLevel::Write),
        "owner" | "admin" | "super_admin" => Some(SourcePermissionLevel::Admin),
        _ => None,
    }
}

/// Map a HubSpot `subscriptionType` to a substrate event.
///
/// Returns `None` for subscription types we don't understand so the
/// caller can skip them without aborting the rest of the batch — see
/// `handle_webhook_event` for why an unknown entry must not discard
/// already-processed valid events.
fn subscription_to_event(
    sub: &str,
    object_id: i64,
    occurred_at: DateTime<Utc>,
    user_id: Option<String>,
    new_role: Option<&str>,
) -> Option<ConnectorEvent> {
    let kind = sub.split_once('.').map_or("", |(prefix, _)| prefix);
    let id = SourceDocumentId::new(format!("{kind}:{object_id}"));
    if sub.ends_with(".creation") {
        Some(ConnectorEvent::DocumentCreated {
            document_id: id,
            occurred_at,
        })
    } else if sub.ends_with(".propertyChange") || sub.ends_with(".update") {
        Some(ConnectorEvent::DocumentUpdated {
            document_id: id,
            occurred_at,
        })
    } else if sub.ends_with(".deletion") {
        Some(ConnectorEvent::DocumentDeleted {
            document_id: id,
            occurred_at,
        })
    } else if sub.ends_with(".permissionChange") {
        Some(ConnectorEvent::PermissionChanged {
            document_id: id,
            user_id: SourceUserId::new(user_id.unwrap_or_default()),
            new_level: new_role.and_then(parse_role),
            occurred_at,
        })
    } else {
        None
    }
}

impl Connector for HubSpotConnector {
    fn authenticate(&self, config: &ConnectorConfig) -> Result<OAuth2Token> {
        let auth_code = config
            .auth_config_json
            .get("authorization_code")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ConnectorError::Auth(
                    "hubspot authenticate: auth_config_json.authorization_code is required".into(),
                )
            })?;
        self.oauth.exchange_code(config, auth_code)
    }

    fn initial_sync(&self, config: &ConnectorConfig, token: &OAuth2Token) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        let kinds = Self::configured_kinds(config)?;
        let mut events: Vec<ConnectorEvent> = Vec::new();
        let mut watermark: Option<DateTime<Utc>> = None;
        for kind in &kinds {
            let objects = self.paginate_list(&base_url, token, *kind)?;
            for obj in &objects {
                events.push(object_to_event(obj, SyncMode::Initial));
                if let Some(t) = obj.updated_at.or(obj.created_at) {
                    watermark = Some(watermark.map_or(t, |w| w.max(t)));
                }
            }
        }
        Ok(SyncRunResult {
            events,
            next_cursor: watermark.map(|t| t.to_rfc3339()),
        })
    }

    fn incremental_sync(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        state: &SyncState,
    ) -> Result<SyncRunResult> {
        let base_url = self.resolved_base_url(config);
        let kinds = Self::configured_kinds(config)?;
        let prior_watermark: Option<DateTime<Utc>> = state
            .cursor
            .as_deref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc));
        // First-time incremental → fall back to "modified since the
        // unix epoch", which the search endpoint accepts as "any
        // modification".
        let cursor_ms = prior_watermark.map_or(0, |t| t.timestamp_millis());
        let mut events: Vec<ConnectorEvent> = Vec::new();
        let mut watermark: Option<DateTime<Utc>> = prior_watermark;
        for kind in &kinds {
            let objects = self.paginate_search(&base_url, token, *kind, cursor_ms)?;
            for obj in &objects {
                // Skip objects that match the cursor exactly — the
                // GTE filter is inclusive, so re-walking the prior
                // sync's last object is expected and not a duplicate.
                if let (Some(prev), Some(t)) = (prior_watermark, obj.updated_at.or(obj.created_at))
                {
                    if t <= prev {
                        continue;
                    }
                }
                events.push(object_to_event(obj, SyncMode::Incremental));
                if let Some(t) = obj.updated_at.or(obj.created_at) {
                    watermark = Some(watermark.map_or(t, |w| w.max(t)));
                }
            }
        }
        Ok(SyncRunResult {
            events,
            next_cursor: watermark.map(|t| t.to_rfc3339()),
        })
    }

    fn fetch_content(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        document_id: &SourceDocumentId,
    ) -> Result<FetchedContent> {
        let base_url = self.resolved_base_url(config);
        // Document ids are minted as `{kind}:{object_id}` (e.g.
        // `note:123`, `contact:456`).
        let raw = document_id.as_str();
        let (kind_str_part, object_id) = raw.split_once(':').ok_or_else(|| {
            ConnectorError::Sync(format!(
                "hubspot fetch_content: malformed document id {raw:?} (expected `kind:id`)"
            ))
        })?;
        let kind = HubSpotObjectKind::from_config_str(kind_str_part).ok_or_else(|| {
            ConnectorError::Sync(format!(
                "hubspot fetch_content: unknown object kind {kind_str_part:?} in id {raw:?}"
            ))
        })?;
        let type_seg = kind.as_path_segment();
        let id_enc = percent_encode_path_component(object_id);

        // Notes carry their body in `hs_note_body` (HTML) and are
        // associated with the CRM records they annotate, so we request
        // those associations and surface them. Other object kinds are
        // rendered from their full property set.
        let detail: HubSpotObjectDetail = if kind == HubSpotObjectKind::Note {
            let url = format!(
                "{base_url}/crm/v3/objects/notes/{id_enc}\
                 ?properties=hs_note_body&associations=contacts,companies,deals"
            );
            bearer_get_json(
                &self.transport,
                "hubspot",
                "/crm/v3/objects/notes/{id}",
                &url,
                token,
                &[],
            )?
        } else {
            // Enumerate the full property catalogue, then request those
            // names explicitly — the object endpoint has no `*` wildcard.
            let names = self.fetch_object_property_names(&base_url, token, type_seg)?;
            let url = if names.is_empty() {
                // No catalogue (or all blank): fall back to the default
                // property set the endpoint returns without the param.
                format!("{base_url}/crm/v3/objects/{type_seg}/{id_enc}")
            } else {
                let props = names
                    .iter()
                    .map(|n| percent_encode_path_component(n))
                    .collect::<Vec<_>>()
                    .join(",");
                format!("{base_url}/crm/v3/objects/{type_seg}/{id_enc}?properties={props}")
            };
            bearer_get_json(
                &self.transport,
                "hubspot",
                "/crm/v3/objects/{type}/{id}",
                &url,
                token,
                &[],
            )?
        };

        // Collect association ids per type (for notes).
        let mut associations: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (assoc_type, group) in &detail.associations {
            let ids: Vec<String> = group
                .results
                .iter()
                .map(|r| r.id.clone())
                .filter(|s| !s.is_empty())
                .collect();
            if !ids.is_empty() {
                associations.insert(assoc_type.clone(), ids);
            }
        }

        let (body, title) = if kind == HubSpotObjectKind::Note {
            let raw_body = detail
                .properties
                .get("hs_note_body")
                .map(property_value_to_string)
                .unwrap_or_default();
            let mut text = strip_html(&raw_body);
            if !associations.is_empty() {
                let mut refs: Vec<String> = associations
                    .iter()
                    .map(|(t, ids)| format!("{t}: {}", ids.join(", ")))
                    .collect();
                refs.sort();
                if !text.is_empty() {
                    text.push_str("\n\n");
                }
                text.push_str("Associated with — ");
                text.push_str(&refs.join("; "));
            }
            (text, None)
        } else {
            // Render every non-empty property as a `key: value` line,
            // sorted for deterministic output.
            let mut lines: Vec<String> = Vec::new();
            for (key, value) in &detail.properties {
                let rendered = property_value_to_string(value);
                if !rendered.is_empty() {
                    lines.push(format!("{key}: {rendered}"));
                }
            }
            let title = hubspot_title(kind, &detail.properties);
            (lines.join("\n"), title)
        };

        let mut metadata = serde_json::json!({
            "provider": "hubspot",
            "object_kind": kind_str(kind),
            "object_id": object_id,
        });
        if !associations.is_empty() {
            metadata["associations"] = serde_json::to_value(&associations).unwrap_or_default();
        }

        let mut fc = FetchedContent::text(body, "text/plain").with_metadata(metadata);
        if let Some(t) = title.filter(|s| !s.is_empty()) {
            fc = fc.with_title(t);
        }
        Ok(fc)
    }

    fn subscribe_webhook(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        callback_url: &str,
    ) -> Result<WebhookSubscription> {
        const SUBSCRIPTION_KINDS: &[&str] = &["creation", "propertyChange", "deletion"];
        let base_url = self.resolved_base_url(config);
        let app_id = Self::configured_app_id(config)?;
        let kinds =
            Self::configured_kinds(config).unwrap_or_else(|_| vec![HubSpotObjectKind::Contact]);
        let mut registered: Vec<String> = Vec::new();
        for object_kind in &kinds {
            for sub_kind in SUBSCRIPTION_KINDS {
                let url = format!("{base_url}/webhooks/v3/{app_id}/subscriptions");
                let event_type = format!("{}.{}", kind_singular(*object_kind), sub_kind);
                let body = serde_json::json!({
                    "eventType": event_type,
                    "active": true,
                    "propertyName": serde_json::Value::Null,
                });
                let resp: HubSpotSubscriptionCreateResponse = match bearer_post_json(
                    &self.transport,
                    "hubspot",
                    "/webhooks/v3/{appId}/subscriptions",
                    &url,
                    token,
                    &[],
                    &body,
                ) {
                    Ok(r) => r,
                    Err(e) => {
                        // Partial-registration rollback: any
                        // already-registered subscription would
                        // otherwise orphan in the HubSpot dashboard,
                        // continuing to deliver webhooks the
                        // substrate can't correlate (because the ids
                        // are about to be discarded when we return
                        // `Err` below). Mirror the Figma pattern at
                        // `figma.rs::rollback_partial_webhooks`.
                        self.rollback_partial_webhooks(&base_url, &app_id, token, &registered);
                        return Err(e);
                    }
                };
                let Some(id) = resp.id else {
                    self.rollback_partial_webhooks(&base_url, &app_id, token, &registered);
                    return Err(ConnectorError::Webhook(format!("hubspot /webhooks/v3/{app_id}/subscriptions returned no id for {event_type}"
                    )));
                };
                registered.push(id.to_string());
            }
        }
        let secret = config
            .auth_config_json
            .get("webhook_secret")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("hubspot-app-secret");
        let mut subscription = WebhookSubscription::new(
            self.instance,
            callback_url,
            WebhookSecret::new(secret),
            WebhookEventTypes::all(),
            // HubSpot subscriptions are evergreen — no provider TTL.
            None,
        );
        if !registered.is_empty() {
            subscription.provider_subscription_id = Some(registered.join(","));
        }
        Ok(subscription)
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        // HubSpot delivers webhooks as a JSON array — a single POST
        // can carry many independent subscription events. Every
        // recognised entry must surface; previously we returned only
        // the first, which silently dropped the rest.
        //
        // Unknown subscription types are skipped rather than
        // aborting the whole batch — when HubSpot adds a new event
        // family we cannot retroactively discard every well-formed
        // event that was queued behind it. Mirrors the OneDrive
        // handler's policy on unknown `changeType`s.
        let batch: Vec<HubSpotWebhookEvent> = serde_json::from_slice(body)?;
        if batch.is_empty() {
            return Err(ConnectorError::Webhook(
                "empty HubSpot webhook batch".to_string(),
            ));
        }
        let mut events: Vec<ConnectorEvent> = Vec::with_capacity(batch.len());
        for e in batch {
            let occurred_at = e
                .occurred_at_ms
                .and_then(DateTime::<Utc>::from_timestamp_millis)
                .unwrap_or_else(Utc::now);
            if let Some(ev) = subscription_to_event(
                &e.subscription_type,
                e.object_id,
                occurred_at,
                e.user_id,
                // HubSpot encodes the new role in `propertyValue`
                // for the `permissionChange` subscription.
                e.property_value.as_deref(),
            ) {
                events.push(ev);
            }
        }
        Ok(events)
    }
}

/// Singular form used in HubSpot subscription event types
/// (`contact.creation`, not `contacts.creation`).
fn kind_singular(k: HubSpotObjectKind) -> &'static str {
    match k {
        HubSpotObjectKind::Contact => "contact",
        HubSpotObjectKind::Company => "company",
        HubSpotObjectKind::Deal => "deal",
        HubSpotObjectKind::Note => "note",
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
                "hubspot-access",
                "hubspot-refresh",
                Utc::now() + Duration::hours(6),
                "crm.objects.contacts.read crm.objects.companies.read crm.objects.deals.read",
            ))
        }
    }

    fn oauth() -> Arc<dyn OAuth2CodeExchange> {
        Arc::new(FixedOAuth)
    }

    fn cfg_with(extra: &serde_json::Value) -> ConnectorConfig {
        let mut base = serde_json::json!({
            "authorization_code": "demo-code",
            "api_base_url": "https://api.test/hubspot",
            "app_id": "12345",
            "webhook_secret": "tenant-app-secret",
        });
        let base_map = base.as_object_mut().unwrap();
        if let Some(extra_map) = extra.as_object() {
            for (k, v) in extra_map {
                base_map.insert(k.clone(), v.clone());
            }
        }
        ConnectorConfig::new(ConnectorKind::HubSpot, AuthKind::OAuth2, ScopeId::new_v4())
            .with_auth_config(base)
    }

    fn cfg() -> ConnectorConfig {
        cfg_with(&serde_json::Value::Object(serde_json::Map::new()))
    }

    fn ok_json(value: &serde_json::Value) -> MockResponse {
        MockResponse::ok_json(serde_json::to_vec(value).unwrap())
    }

    #[test]
    fn authenticate_dispatches_to_oauth_exchange() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = HubSpotConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        assert!(tok.scope.contains("crm.objects.contacts.read"));
    }

    #[test]
    fn authenticate_requires_authorization_code() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = HubSpotConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let cfg_no_code =
            ConnectorConfig::new(ConnectorKind::HubSpot, AuthKind::OAuth2, ScopeId::new_v4());
        let err = c.authenticate(&cfg_no_code).unwrap_err();
        assert!(matches!(err, ConnectorError::Auth(_)));
    }

    #[test]
    fn initial_sync_emits_create_per_object_and_follows_paging() {
        let now = Utc::now();
        let transport = Arc::new(MockHttpTransport::new());
        // Page 1 — paging.next.after points to "next-token"
        transport.expect(
            HttpMethod::Get,
            "https://api.test/hubspot/crm/v3/objects/contacts?limit=100",
            ok_json(&serde_json::json!({
                "results": [{
                    "id": "101",
                    "createdAt": now,
                    "updatedAt": now,
                }],
                "paging": {"next": {"after": "next-token"}}
            })),
        );
        // Page 2 — no more.
        transport.expect(
            HttpMethod::Get,
            "https://api.test/hubspot/crm/v3/objects/contacts?limit=100&after=next-token",
            ok_json(&serde_json::json!({
                "results": [{
                    "id": "102",
                    "createdAt": now,
                    "updatedAt": now,
                }]
            })),
        );
        let c = HubSpotConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 2);
        assert!(matches!(
            res.events[0],
            ConnectorEvent::DocumentCreated { .. }
        ));
        assert!(matches!(
            res.events[1],
            ConnectorEvent::DocumentCreated { .. }
        ));
        assert!(res.next_cursor.is_some());
    }

    #[test]
    fn initial_sync_walks_multiple_kinds_when_configured() {
        let now = Utc::now();
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/hubspot/crm/v3/objects/contacts?limit=100",
            ok_json(&serde_json::json!({
                "results": [{
                    "id": "1",
                    "createdAt": now,
                    "updatedAt": now,
                }]
            })),
        );
        transport.expect(
            HttpMethod::Get,
            "https://api.test/hubspot/crm/v3/objects/companies?limit=100",
            ok_json(&serde_json::json!({
                "results": [{
                    "id": "2",
                    "createdAt": now,
                    "updatedAt": now,
                }]
            })),
        );
        let c = HubSpotConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c
            .initial_sync(
                &cfg_with(&serde_json::json!({"object_kinds": ["contacts", "companies"]})),
                &tok,
            )
            .unwrap();
        assert_eq!(res.events.len(), 2);
        let ids: Vec<&str> = res
            .events
            .iter()
            .filter_map(|e| match e {
                ConnectorEvent::DocumentCreated { document_id, .. } => Some(document_id.as_str()),
                _ => None,
            })
            .collect();
        assert!(ids.iter().any(|s| s.starts_with("contact:")));
        assert!(ids.iter().any(|s| s.starts_with("company:")));
    }

    #[test]
    fn initial_sync_rejects_unknown_object_kind() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = HubSpotConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let err = c
            .initial_sync(
                &cfg_with(&serde_json::json!({"object_kinds": ["weird_kind"]})),
                &tok,
            )
            .unwrap_err();
        assert!(matches!(err, ConnectorError::Sync(_)));
    }

    #[test]
    fn incremental_sync_filters_via_search_endpoint() {
        // Cursor at now-1h; the mock returns three rows but the
        // <= cursor row must be dropped client-side (GTE is
        // inclusive).
        let now = Utc::now();
        let cursor = (now - Duration::hours(1)).to_rfc3339();
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            "https://api.test/hubspot/crm/v3/objects/contacts/search",
            ok_json(&serde_json::json!({
                "results": [
                    {
                        "id": "old",
                        "updatedAt": now - Duration::hours(1),
                    },
                    {
                        "id": "new",
                        "updatedAt": now,
                    },
                    {
                        "id": "archived",
                        "updatedAt": now,
                        "archived": true,
                    }
                ]
            })),
        );
        let c = HubSpotConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let mut state = SyncState::new(c.instance);
        state.cursor = Some(cursor);
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        // "old" matches GTE-cursor exactly → dropped, "new" emits
        // Updated, "archived" emits Deleted.
        assert_eq!(res.events.len(), 2);
        assert!(matches!(
            res.events[0],
            ConnectorEvent::DocumentUpdated { .. }
        ));
        assert!(matches!(
            res.events[1],
            ConnectorEvent::DocumentDeleted { .. }
        ));
        // The body must include the lastmodifieddate filter.
        let recorded = transport.recorded();
        let body: serde_json::Value = serde_json::from_slice(&recorded[0].body).unwrap();
        assert_eq!(
            body["filterGroups"][0]["filters"][0]["propertyName"],
            "hs_lastmodifieddate"
        );
        assert_eq!(body["filterGroups"][0]["filters"][0]["operator"], "GTE");
    }

    #[test]
    fn list_401_propagates_as_auth_error() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/hubspot/crm/v3/objects/contacts?limit=100",
            MockResponse::status(401, b"unauthorized".to_vec()),
        );
        let c = HubSpotConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let err = c.initial_sync(&cfg(), &tok).unwrap_err();
        assert!(matches!(err, ConnectorError::Auth(_)));
    }

    #[test]
    fn subscribe_webhook_posts_one_per_event_type_and_captures_ids() {
        let transport = Arc::new(MockHttpTransport::new());
        // 3 subscription kinds × 1 object kind = 3 POSTs.
        for id in [10_i64, 11, 12] {
            transport.expect(
                HttpMethod::Post,
                "https://api.test/hubspot/webhooks/v3/12345/subscriptions",
                ok_json(&serde_json::json!({"id": id, "active": true})),
            );
        }
        let c = HubSpotConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let sub = c
            .subscribe_webhook(&cfg(), &tok, "https://demo.example/webhooks/hubspot")
            .unwrap();
        assert_eq!(sub.provider_subscription_id.as_deref(), Some("10,11,12"));
        // Every POST must carry the configured eventType derived
        // from (object_kind, sub_kind).
        let recorded = transport.recorded();
        let event_types: Vec<String> = recorded
            .iter()
            .map(|r| {
                let b: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
                b["eventType"].as_str().unwrap().to_string()
            })
            .collect();
        assert_eq!(
            event_types,
            vec![
                "contact.creation".to_string(),
                "contact.propertyChange".to_string(),
                "contact.deletion".to_string(),
            ]
        );
    }

    #[test]
    fn subscribe_webhook_requires_app_id() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = HubSpotConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let cfg_no_app =
            ConnectorConfig::new(ConnectorKind::HubSpot, AuthKind::OAuth2, ScopeId::new_v4())
                .with_auth_config(serde_json::json!({
                    "authorization_code": "demo-code",
                    "api_base_url": "https://api.test/hubspot",
                }));
        let err = c
            .subscribe_webhook(&cfg_no_app, &tok, "https://demo.example/webhooks/hubspot")
            .unwrap_err();
        assert!(matches!(err, ConnectorError::Webhook(_)));
    }

    #[test]
    fn subscribe_webhook_rolls_back_successful_registrations_on_failure() {
        // HubSpot's `subscribe_webhook` iterates the
        // (objectType × subscriptionType) matrix issuing one POST
        // per pair. Single object kind (contacts) × 3 subscription
        // kinds (creation, propertyChange, deletion) = 3 POSTs.
        // Mock first two POSTs success (`10`, `11`), third POST
        // returns 500. After the failure, the connector MUST issue
        // DELETE requests for `10` and `11` before returning Err.
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            "https://api.test/hubspot/webhooks/v3/12345/subscriptions",
            ok_json(&serde_json::json!({"id": 10_i64, "active": true})),
        );
        transport.expect(
            HttpMethod::Post,
            "https://api.test/hubspot/webhooks/v3/12345/subscriptions",
            ok_json(&serde_json::json!({"id": 11_i64, "active": true})),
        );
        transport.expect(
            HttpMethod::Post,
            "https://api.test/hubspot/webhooks/v3/12345/subscriptions",
            MockResponse::status(500, b"internal server error".to_vec()),
        );
        // Rollback DELETEs — these are the assertions we care about.
        transport.expect(
            HttpMethod::Delete,
            "https://api.test/hubspot/webhooks/v3/12345/subscriptions/10",
            MockResponse::status(204, Vec::new()),
        );
        transport.expect(
            HttpMethod::Delete,
            "https://api.test/hubspot/webhooks/v3/12345/subscriptions/11",
            MockResponse::status(204, Vec::new()),
        );

        let c = HubSpotConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let err = c
            .subscribe_webhook(&cfg(), &tok, "https://demo.example/webhooks/hubspot")
            .unwrap_err();
        assert!(
            matches!(
                err,
                ConnectorError::Sync(_) | ConnectorError::Webhook(_) | ConnectorError::Auth(_)
            ),
            "subscribe_webhook must surface the upstream failure"
        );
        // Verify that both rollback DELETEs were actually issued.
        let deletes: Vec<_> = transport
            .recorded()
            .into_iter()
            .filter(|r| r.method == HttpMethod::Delete)
            .collect();
        assert_eq!(
            deletes.len(),
            2,
            "must issue one DELETE per successfully-registered subscription id"
        );
        assert_eq!(
            deletes[0].url,
            "https://api.test/hubspot/webhooks/v3/12345/subscriptions/10"
        );
        assert_eq!(
            deletes[1].url,
            "https://api.test/hubspot/webhooks/v3/12345/subscriptions/11"
        );
    }

    #[test]
    fn subscribe_webhook_rolls_back_when_server_omits_id() {
        // Defence-in-depth: even if a POST returns 200 with no `id`
        // field, the connector must still tear down everything it
        // *did* register before failing.
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            "https://api.test/hubspot/webhooks/v3/12345/subscriptions",
            ok_json(&serde_json::json!({"id": 20_i64, "active": true})),
        );
        // Second POST: 200 OK but no `id` — connector must rollback.
        transport.expect(
            HttpMethod::Post,
            "https://api.test/hubspot/webhooks/v3/12345/subscriptions",
            ok_json(&serde_json::json!({"active": true})),
        );
        transport.expect(
            HttpMethod::Delete,
            "https://api.test/hubspot/webhooks/v3/12345/subscriptions/20",
            MockResponse::status(204, Vec::new()),
        );
        let c = HubSpotConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let err = c
            .subscribe_webhook(&cfg(), &tok, "https://demo.example/webhooks/hubspot")
            .unwrap_err();
        assert!(matches!(err, ConnectorError::Webhook(_)));
        let deletes: Vec<_> = transport
            .recorded()
            .into_iter()
            .filter(|r| r.method == HttpMethod::Delete)
            .collect();
        assert_eq!(deletes.len(), 1);
        assert_eq!(
            deletes[0].url,
            "https://api.test/hubspot/webhooks/v3/12345/subscriptions/20"
        );
    }

    #[test]
    fn webhook_parses_contact_creation() {
        let body = serde_json::json!([
            {
                "subscriptionType": "contact.creation",
                "objectId": 1234,
                "occurredAt": Utc::now().timestamp_millis(),
            }
        ]);
        let transport = Arc::new(MockHttpTransport::new());
        let c = HubSpotConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            ConnectorEvent::DocumentCreated { document_id, .. } => {
                assert_eq!(document_id.as_str(), "contact:1234");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn webhook_parses_permission_change() {
        let body = serde_json::json!([
            {
                "subscriptionType": "company.permissionChange",
                "objectId": 42,
                "occurredAt": Utc::now().timestamp_millis(),
                "userId": "u-1",
                "propertyValue": "editor",
            }
        ]);
        let transport = Arc::new(MockHttpTransport::new());
        let c = HubSpotConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            ConnectorEvent::PermissionChanged {
                document_id,
                new_level,
                ..
            } => {
                assert_eq!(document_id.as_str(), "company:42");
                assert_eq!(*new_level, Some(SourcePermissionLevel::Write));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn webhook_empty_batch_errors() {
        let body = serde_json::json!([]);
        let transport = Arc::new(MockHttpTransport::new());
        let c = HubSpotConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let err = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap_err();
        assert!(matches!(err, ConnectorError::Webhook(_)));
    }

    #[test]
    fn webhook_unknown_subscription_is_skipped_not_errored() {
        // Regression: an unknown `subscriptionType` previously
        // bubbled up as `Err` from `subscription_to_event` via the
        // `?` operator inside `handle_webhook_event`, which would
        // have discarded every valid event already queued earlier in
        // the same batch. The handler must now skip the unknown
        // entry and continue processing the remainder.
        let body = serde_json::json!([
            {
                "subscriptionType": "contact.creation",
                "objectId": 1,
                "occurredAt": Utc::now().timestamp_millis(),
            },
            {
                "subscriptionType": "foo.weird",
                "objectId": 2,
                "occurredAt": Utc::now().timestamp_millis(),
            },
            {
                "subscriptionType": "deal.deletion",
                "objectId": 9,
                "occurredAt": Utc::now().timestamp_millis(),
            }
        ]);
        let transport = Arc::new(MockHttpTransport::new());
        let c = HubSpotConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert_eq!(
            evs.len(),
            2,
            "valid events on either side of an unknown subscriptionType must still surface",
        );
        assert!(matches!(evs[0], ConnectorEvent::DocumentCreated { .. }));
        assert!(matches!(evs[1], ConnectorEvent::DocumentDeleted { .. }));
    }

    #[test]
    fn webhook_emits_every_event_in_batched_payload() {
        // Regression test: HubSpot ships subscription events in a
        // JSON array — one POST can carry many. Earlier revisions
        // dropped everything past index 0.
        let body = serde_json::json!([
            {
                "subscriptionType": "contact.creation",
                "objectId": 1,
                "occurredAt": Utc::now().timestamp_millis(),
            },
            {
                "subscriptionType": "contact.propertyChange",
                "objectId": 1,
                "occurredAt": Utc::now().timestamp_millis(),
            },
            {
                "subscriptionType": "deal.deletion",
                "objectId": 9,
                "occurredAt": Utc::now().timestamp_millis(),
            }
        ]);
        let transport = Arc::new(MockHttpTransport::new());
        let c = HubSpotConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let evs = c
            .handle_webhook_event(&serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert_eq!(evs.len(), 3, "every batched HubSpot event must surface");
        assert!(matches!(evs[0], ConnectorEvent::DocumentCreated { .. }));
        assert!(matches!(evs[1], ConnectorEvent::DocumentUpdated { .. }));
        assert!(matches!(evs[2], ConnectorEvent::DocumentDeleted { .. }));
    }

    // ───────────── fetch_content ─────────────

    #[test]
    fn fetch_content_renders_contact_properties() {
        let transport = Arc::new(MockHttpTransport::new());
        // The catalogue is enumerated first; its names drive the (sorted,
        // comma-separated) `properties` query on the object fetch.
        transport.expect(
            HttpMethod::Get,
            "https://api.test/hubspot/crm/v3/properties/contacts",
            ok_json(&serde_json::json!({
                "results": [
                    { "name": "firstname" },
                    { "name": "lastname" },
                    { "name": "email" },
                    { "name": "company" },
                    { "name": "empty_field" }
                ]
            })),
        );
        transport.expect(
            HttpMethod::Get,
            "https://api.test/hubspot/crm/v3/objects/contacts/501\
             ?properties=company,email,empty_field,firstname,lastname",
            ok_json(&serde_json::json!({
                "id": "501",
                "properties": {
                    "firstname": "Ada",
                    "lastname": "Lovelace",
                    "email": "ada@example.com",
                    "company": "Analytical Engines",
                    "empty_field": serde_json::Value::Null
                }
            })),
        );
        let c = HubSpotConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let fc = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("contact:501"))
            .unwrap();
        let body = String::from_utf8(fc.body).unwrap();
        assert!(body.contains("email: ada@example.com"));
        assert!(body.contains("firstname: Ada"));
        // Null-valued properties are dropped.
        assert!(!body.contains("empty_field"));
        assert_eq!(fc.mime_type, "text/plain");
        assert_eq!(fc.title.as_deref(), Some("Ada Lovelace"));
        assert_eq!(fc.metadata["object_kind"], serde_json::json!("contact"));
        assert_eq!(fc.metadata["object_id"], serde_json::json!("501"));
    }

    #[test]
    fn fetch_content_note_strips_html_and_lists_associations() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/hubspot/crm/v3/objects/notes/77?properties=hs_note_body&associations=contacts,companies,deals",
            ok_json(&serde_json::json!({
                "id": "77",
                "properties": { "hs_note_body": "<p>Called <strong>Ada</strong> re: renewal.</p>" },
                "associations": {
                    "contacts": { "results": [ { "id": "501" }, { "id": "502" } ] },
                    "companies": { "results": [ { "id": "900" } ] }
                }
            })),
        );
        let c = HubSpotConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let fc = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("note:77"))
            .unwrap();
        let body = String::from_utf8(fc.body).unwrap();
        assert!(body.contains("Called Ada re: renewal."));
        assert!(body.contains("Associated with —"));
        assert!(body.contains("contacts: 501, 502"));
        assert!(body.contains("companies: 900"));
        assert!(fc.title.is_none());
        assert_eq!(
            fc.metadata["associations"]["contacts"],
            serde_json::json!(["501", "502"])
        );
    }

    #[test]
    fn fetch_content_rejects_malformed_document_id() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = HubSpotConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let err = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("no-delimiter"))
            .unwrap_err();
        assert!(matches!(err, ConnectorError::Sync(_)));
    }

    #[test]
    fn fetch_content_maps_404_to_sync_error() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/hubspot/crm/v3/properties/deals",
            ok_json(&serde_json::json!({ "results": [ { "name": "dealname" } ] })),
        );
        transport.expect(
            HttpMethod::Get,
            "https://api.test/hubspot/crm/v3/objects/deals/404?properties=dealname",
            MockResponse::status(
                404,
                br#"{"status":"error","category":"OBJECT_NOT_FOUND"}"#.to_vec(),
            ),
        );
        let c = HubSpotConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let err = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("deal:404"))
            .unwrap_err();
        assert!(matches!(err, ConnectorError::Sync(_)));
    }

    #[test]
    fn fetch_content_maps_429_to_sync_error() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/hubspot/crm/v3/properties/companies",
            ok_json(&serde_json::json!({ "results": [ { "name": "name" } ] })),
        );
        transport.expect(
            HttpMethod::Get,
            "https://api.test/hubspot/crm/v3/objects/companies/9?properties=name",
            MockResponse::status(429, b"rate limited".to_vec()),
        );
        let c = HubSpotConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let err = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("company:9"))
            .unwrap_err();
        assert!(matches!(err, ConnectorError::Sync(_)));
    }

    #[test]
    fn fetch_content_falls_back_to_default_properties_when_catalogue_empty() {
        // An empty property catalogue must not produce a `properties=`
        // param — the object is fetched with the endpoint's defaults.
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Get,
            "https://api.test/hubspot/crm/v3/properties/companies",
            ok_json(&serde_json::json!({ "results": [] })),
        );
        transport.expect(
            HttpMethod::Get,
            "https://api.test/hubspot/crm/v3/objects/companies/9",
            ok_json(&serde_json::json!({
                "id": "9",
                "properties": { "name": "Globex" }
            })),
        );
        let c = HubSpotConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let fc = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("company:9"))
            .unwrap();
        let body = String::from_utf8(fc.body).unwrap();
        assert!(body.contains("name: Globex"));
        assert_eq!(fc.title.as_deref(), Some("Globex"));
    }
}
