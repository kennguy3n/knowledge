//! Cdiscount connector — Cdiscount Marketplace API (SOAP/WCF at
//! `https://wsvc.cdiscount.com/MarketplaceAPIService.svc`).
//!
//! Cdiscount — French marketplace seller API (orders). Unlike the
//! other connectors in this batch, Cdiscount's Marketplace API is a
//! SOAP 1.1 / WCF service, *not* a JSON REST API: the host
//! `api.cdiscount.com` used by the original template does not resolve,
//! whereas the real service is published at
//! `wsvc.cdiscount.com/MarketplaceAPIService.svc` (WSDL reachable at
//! `?wsdl`). Orders are retrieved with the `GetOrderList` operation.
//!
//! Authentication is by Cdiscount seller token: the token is read from
//! `auth_config_json.api_key` and carried inside the SOAP
//! `headerMessage` (`Security/TokenId`), *not* in an HTTP header — the
//! original `X-Cdiscount-Api-Key` header does nothing. (A rotating
//! `authorization_code` grant falls back to the injected
//! [`OAuth2CodeExchange`] to mint the token.)
//!
//! * `initial_sync` calls `GetOrderList` with an empty filter, emits
//!   `DocumentCreated` per order and tracks the maximum `CreationDate`
//!   as an RFC-3339 watermark.
//! * `incremental_sync` re-calls `GetOrderList` with the
//!   `BeginCreationDate` filter set to the stored watermark and emits
//!   `DocumentUpdated`, deduping the inclusive boundary order. (The
//!   GetOrderList filter keys off creation date; state changes to
//!   already-seen orders are surfaced via webhooks.)
//! * `fetch_content` calls `GetOrderList` filtered by
//!   `OrderReferenceList` for the single order number.
//! * Webhooks are configured in the provider dashboard, so
//!   `subscribe_webhook` records a polling-only subscription.
//! * `handle_webhook_event` parses the delivered payload.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use connector_framework::{
    classify_failure, Connector, ConnectorConfig, ConnectorError, ConnectorEvent,
    ConnectorInstanceId, FetchedContent, HttpRequest, HttpTransport, OAuth2CodeExchange,
    OAuth2Token, Result, SourceDocumentId, SyncRunResult, SyncState, WatermarkCursor,
    WebhookEventTypes, WebhookSecret, WebhookSubscription,
};

/// Default Cdiscount Marketplace API SOAP endpoint.
pub const DEFAULT_API_BASE_URL: &str = "https://wsvc.cdiscount.com/MarketplaceAPIService.svc";
/// Default scope recorded on the synthesised token.
pub const DEFAULT_SCOPE: &str = "orders";
/// `OAuth2Token::token_type` marker for a static seller token. Kept for
/// API compatibility; the token is placed in the SOAP header regardless
/// of provenance, so this marker is not used to pick an HTTP header.
pub const API_KEY_TOKEN_TYPE: &str = "ApiKey";
/// SOAPAction for the `GetOrderList` operation.
const GET_ORDER_LIST_ACTION: &str = "http://www.cdiscount.com/IMarketplaceAPIService/GetOrderList";
/// Cdiscount service contract namespace (`tns`).
const NS_TNS: &str = "http://www.cdiscount.com";
/// Datacontract namespace for the shared `HeaderMessage` types.
const NS_MSG: &str =
    "http://schemas.datacontract.org/2004/07/Cdiscount.Framework.Core.Communication.Messages";
/// Default Cdiscount France `CatalogID` for the request context.
pub const DEFAULT_CATALOG_ID: i64 = 1;
/// Default Cdiscount France `SiteID` for the request context.
pub const DEFAULT_SITE_ID: i64 = 100;

/// A single parsed order from a `GetOrderListResponse`.
#[derive(Debug, Clone, Default)]
struct CdiscountOrder {
    order_number: String,
    order_state: Option<String>,
    creation_date: Option<String>,
    modified_date: Option<String>,
}

impl CdiscountOrder {
    /// The change watermark: prefer `ModifiedDate`, fall back to
    /// `CreationDate`.
    fn watermark(&self) -> Option<DateTime<Utc>> {
        self.modified_date
            .as_deref()
            .and_then(parse_rfc3339)
            .or_else(|| self.creation_date.as_deref().and_then(parse_rfc3339))
    }

    /// The filterable creation timestamp.
    fn created_at(&self) -> Option<DateTime<Utc>> {
        self.creation_date.as_deref().and_then(parse_rfc3339)
    }
}

/// Cdiscount connector.
pub struct CdiscountConnector {
    /// Connector instance id.
    pub instance: ConnectorInstanceId,
    transport: Arc<dyn HttpTransport>,
    oauth: Arc<dyn OAuth2CodeExchange>,
    api_base_url: String,
}

impl std::fmt::Debug for CdiscountConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CdiscountConnector")
            .field("instance", &self.instance)
            .field("api_base_url", &self.api_base_url)
            .field("transport", &"<HttpTransport>")
            .field("oauth", &"<OAuth2CodeExchange>")
            .finish()
    }
}

impl CdiscountConnector {
    /// Construct a Cdiscount connector.
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

    /// Override the Cdiscount API base URL.
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

    fn context_ids(config: &ConnectorConfig) -> (i64, i64) {
        let catalog = config
            .auth_config_json
            .get("catalog_id")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(DEFAULT_CATALOG_ID);
        let site = config
            .auth_config_json
            .get("site_id")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(DEFAULT_SITE_ID);
        (catalog, site)
    }

    /// Execute a `GetOrderList` SOAP call and return the parsed orders.
    fn get_order_list(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        begin_creation_date: Option<&str>,
        order_reference: Option<&str>,
    ) -> Result<Vec<CdiscountOrder>> {
        let base_url = self.resolved_base_url(config);
        let (catalog_id, site_id) = Self::context_ids(config);
        let envelope = build_get_order_list_envelope(
            token.access_token.expose(),
            catalog_id,
            site_id,
            begin_creation_date,
            order_reference,
        );
        let req = HttpRequest::post(&base_url, envelope.into_bytes())
            .with_header("Content-Type", "text/xml; charset=utf-8")
            .with_header("SOAPAction", format!("\"{GET_ORDER_LIST_ACTION}\""));
        let resp = self.transport.execute(req)?;
        if !resp.is_success() {
            return Err(classify_failure("cdiscount", "GetOrderList", &resp));
        }
        let xml = String::from_utf8_lossy(&resp.body);
        Ok(parse_orders(&xml))
    }
}

fn parse_rfc3339(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

fn id_value_to_string(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) if !s.is_empty() => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// XML-escape a text value for safe inclusion in a SOAP element.
fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// Build the SOAP 1.1 envelope for a `GetOrderList` request. The token
/// is carried in `headerMessage/Security/TokenId`; `orderFilter`
/// optionally constrains by creation date and/or order reference.
fn build_get_order_list_envelope(
    token_id: &str,
    catalog_id: i64,
    site_id: i64,
    begin_creation_date: Option<&str>,
    order_reference: Option<&str>,
) -> String {
    use std::fmt::Write as _;
    let mut filter = String::new();
    // OrderFilter members are emitted in DataContract (alphabetical)
    // order: BeginCreationDate, OrderReferenceList, States.
    if let Some(date) = begin_creation_date {
        let _ = write!(
            filter,
            "<tns:BeginCreationDate>{}</tns:BeginCreationDate>",
            xml_escape(date)
        );
    }
    if let Some(reference) = order_reference {
        let _ = write!(
            filter,
            "<tns:OrderReferenceList xmlns:arr=\"http://schemas.microsoft.com/2003/10/Serialization/Arrays\">\
<arr:string>{}</arr:string></tns:OrderReferenceList>",
            xml_escape(reference)
        );
    }
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
<soap:Envelope xmlns:soap=\"http://schemas.xmlsoap.org/soap/envelope/\" \
xmlns:tns=\"{NS_TNS}\" xmlns:msg=\"{NS_MSG}\">\
<soap:Body>\
<tns:GetOrderList>\
<tns:headerMessage>\
<msg:Context><msg:CatalogID>{catalog_id}</msg:CatalogID><msg:SiteID>{site_id}</msg:SiteID></msg:Context>\
<msg:Security><msg:TokenId>{token}</msg:TokenId></msg:Security>\
<msg:Version>1.0</msg:Version>\
</tns:headerMessage>\
<tns:orderFilter>{filter}</tns:orderFilter>\
</tns:GetOrderList>\
</soap:Body>\
</soap:Envelope>",
        token = xml_escape(token_id),
    )
}

/// Find the next opening tag (namespace-prefix agnostic) whose local
/// name is `local`, starting at byte offset `from`. Returns
/// `(start_of_'<', offset_just_after_'>')`, skipping self-closing tags.
fn open_tag(xml: &str, local: &str, from: usize) -> Option<(usize, usize)> {
    let mut i = from;
    while let Some(rel) = xml[i..].find('<') {
        let lt = i + rel;
        let after = &xml[lt + 1..];
        if after.starts_with('/') || after.starts_with('!') || after.starts_with('?') {
            i = lt + 1;
            continue;
        }
        let gt_rel = after.find('>')?;
        let gt = lt + 1 + gt_rel;
        let tag = &after[..gt_rel];
        let self_closing = tag.ends_with('/');
        let name_end = tag
            .find(|c: char| c.is_whitespace() || c == '/')
            .unwrap_or(tag.len());
        let qname = &tag[..name_end];
        let lname = qname.rsplit(':').next().unwrap_or(qname);
        if lname == local && !self_closing {
            return Some((lt, gt + 1));
        }
        i = gt + 1;
    }
    None
}

/// Find the closing tag `</...local>` that matches the element already
/// opened immediately before `from`, returning the offset of its `<`.
///
/// The scan is depth-aware: nested elements that share `local`'s local
/// name increment the depth and their closes are skipped, so the offset
/// returned is the *matching* close rather than the first one. Without
/// this, an element containing a same-named child (e.g. an `Order`
/// nesting another `Order`) would be truncated at the inner close.
fn close_tag(xml: &str, local: &str, from: usize) -> Option<usize> {
    let mut i = from;
    let mut depth = 1usize;
    while let Some(rel) = xml[i..].find('<') {
        let lt = i + rel;
        let after = &xml[lt + 1..];
        if let Some(rest) = after.strip_prefix('/') {
            // Closing tag `</...>`. `gt_rel` is measured from `after`
            // (the `<`-relative slice) so the `i = lt + 1 + gt_rel + 1`
            // advancement below is identical to the comment and
            // opening-tag branches; `rest` is only used for the name.
            let gt_rel = after.find('>')?;
            let name_end = rest
                .find(|c: char| c.is_whitespace() || c == '>')
                .unwrap_or(rest.len());
            let lname = rest[..name_end]
                .rsplit(':')
                .next()
                .unwrap_or(&rest[..name_end]);
            if lname == local {
                depth -= 1;
                if depth == 0 {
                    return Some(lt);
                }
            }
            i = lt + 1 + gt_rel + 1;
            continue;
        }
        if after.starts_with('!') || after.starts_with('?') {
            // Comment / processing instruction / declaration.
            let gt_rel = after.find('>')?;
            i = lt + 1 + gt_rel + 1;
            continue;
        }
        // Opening tag `<...>`.
        let gt_rel = after.find('>')?;
        let tag = &after[..gt_rel];
        let self_closing = tag.ends_with('/');
        let name_end = tag
            .find(|c: char| c.is_whitespace() || c == '/')
            .unwrap_or(tag.len());
        let lname = tag[..name_end]
            .rsplit(':')
            .next()
            .unwrap_or(&tag[..name_end]);
        if lname == local && !self_closing {
            depth += 1;
        }
        i = lt + 1 + gt_rel + 1;
    }
    None
}

/// Return the trimmed, unescaped text of the first element whose local
/// name is `local` within `xml`.
fn first_text(xml: &str, local: &str) -> Option<String> {
    let (_, content_start) = open_tag(xml, local, 0)?;
    let close = close_tag(xml, local, content_start)?;
    let raw = &xml[content_start..close];
    let unescaped = raw
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&");
    let trimmed = unescaped.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Parse every `<Order>` element out of a `GetOrderListResponse`.
fn parse_orders(xml: &str) -> Vec<CdiscountOrder> {
    let mut orders = Vec::new();
    let mut cursor = 0;
    while let Some((_, content_start)) = open_tag(xml, "Order", cursor) {
        let Some(close) = close_tag(xml, "Order", content_start) else {
            break;
        };
        let block = &xml[content_start..close];
        let order_number = first_text(block, "OrderNumber").unwrap_or_default();
        if !order_number.is_empty() {
            orders.push(CdiscountOrder {
                order_number,
                order_state: first_text(block, "OrderState"),
                creation_date: first_text(block, "CreationDate"),
                modified_date: first_text(block, "ModifiedDate"),
            });
        }
        // Advance past this order's closing tag.
        cursor = close + 2;
    }
    orders
}

impl Connector for CdiscountConnector {
    fn authenticate(&self, config: &ConnectorConfig) -> Result<OAuth2Token> {
        if let Some(api_key) = config
            .auth_config_json
            .get("api_key")
            .and_then(serde_json::Value::as_str)
        {
            let mut token = OAuth2Token::new_without_refresh(
                api_key,
                Utc::now() + chrono::Duration::days(365),
                DEFAULT_SCOPE,
            );
            token.token_type = API_KEY_TOKEN_TYPE.to_string();
            return Ok(token);
        }
        let auth_code = config
            .auth_config_json
            .get("authorization_code")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ConnectorError::Auth(
                    "cdiscount authenticate: auth_config_json.api_key or .authorization_code is required"
                        .into(),
                )
            })?;
        self.oauth.exchange_code(config, auth_code)
    }

    fn initial_sync(&self, config: &ConnectorConfig, token: &OAuth2Token) -> Result<SyncRunResult> {
        let orders = self.get_order_list(config, token, None, None)?;
        let mut events = Vec::with_capacity(orders.len());
        let mut cursor = WatermarkCursor::empty();
        for order in &orders {
            let occurred_at = order.watermark().unwrap_or_else(Utc::now);
            events.push(ConnectorEvent::DocumentCreated {
                document_id: SourceDocumentId::new(order.order_number.clone()),
                occurred_at,
            });
            if let Some(t) = order.created_at() {
                cursor.observe(t, &order.order_number);
            }
        }
        Ok(SyncRunResult {
            events,
            next_cursor: cursor.to_cursor_string(),
        })
    }

    fn incremental_sync(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        state: &SyncState,
    ) -> Result<SyncRunResult> {
        let prior = WatermarkCursor::parse(state.cursor.as_deref());
        let since = prior.query_since();
        let orders = self.get_order_list(config, token, since.as_deref(), None)?;
        let mut events = Vec::new();
        let mut cursor = prior.clone();
        for order in &orders {
            let Some(created) = order.created_at() else {
                continue;
            };
            // Dedup the inclusive boundary order returned by the
            // BeginCreationDate filter while still surfacing a brand-new
            // order sharing that exact instant.
            if !prior.should_emit(created, &order.order_number) {
                continue;
            }
            events.push(ConnectorEvent::DocumentUpdated {
                document_id: SourceDocumentId::new(order.order_number.clone()),
                occurred_at: order.watermark().unwrap_or(created),
            });
            cursor.observe(created, &order.order_number);
        }
        Ok(SyncRunResult {
            events,
            next_cursor: cursor.to_cursor_string(),
        })
    }

    fn fetch_content(
        &self,
        config: &ConnectorConfig,
        token: &OAuth2Token,
        document_id: &SourceDocumentId,
    ) -> Result<FetchedContent> {
        let order_number = document_id.as_str();
        let orders = self.get_order_list(config, token, None, Some(order_number))?;
        let order = orders
            .into_iter()
            .find(|o| o.order_number == order_number)
            .ok_or_else(|| {
                ConnectorError::Sync(format!("cdiscount order {order_number} not found"))
            })?;
        let state = order.order_state.as_deref().unwrap_or("unknown");
        let body = format!(
            "# Cdiscount order {order_number}\n\nState: {state}\nCreated: {}\n",
            order.creation_date.as_deref().unwrap_or("")
        );
        Ok(FetchedContent::text(body, "text/markdown")
            .with_title(format!("Cdiscount order {order_number}"))
            .with_metadata(serde_json::json!({
                "provider": "cdiscount",
                "record_id": order.order_number,
                "order_state": order.order_state,
                "modified_date": order.modified_date,
            })))
    }

    fn subscribe_webhook(
        &self,
        config: &ConnectorConfig,
        _token: &OAuth2Token,
        callback_url: &str,
    ) -> Result<WebhookSubscription> {
        // Cdiscount order notifications are configured in the seller
        // dashboard; no SOAP operation creates them. Record a
        // polling-only subscription so the runtime falls back to
        // incremental_sync.
        let secret = config
            .auth_config_json
            .get("webhook_secret")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("cdiscount-webhook-secret");
        Ok(WebhookSubscription::new(
            self.instance,
            callback_url,
            WebhookSecret::new(secret),
            WebhookEventTypes::all(),
            None,
        ))
    }

    fn handle_webhook_event(&self, body: &[u8]) -> Result<Vec<ConnectorEvent>> {
        let deliveries: Vec<CdiscountWebhookEvent> =
            if let Ok(batch) = serde_json::from_slice::<Vec<CdiscountWebhookEvent>>(body) {
                batch
            } else {
                vec![
                    serde_json::from_slice::<CdiscountWebhookEvent>(body).map_err(|e| {
                        ConnectorError::Webhook(format!("cdiscount webhook parse failed: {e}"))
                    })?,
                ]
            };
        if deliveries.is_empty() {
            return Err(ConnectorError::Webhook(
                "empty cdiscount webhook batch".into(),
            ));
        }
        let mut events = Vec::with_capacity(deliveries.len());
        for delivery in deliveries {
            let id_str = id_value_to_string(&delivery.order_id).ok_or_else(|| {
                ConnectorError::Webhook("cdiscount webhook event missing order id".into())
            })?;
            let id = SourceDocumentId::new(id_str);
            let occurred_at = Utc::now();
            let event_type = delivery.event.to_ascii_lowercase();
            let event = if event_type.contains("create") {
                ConnectorEvent::DocumentCreated {
                    document_id: id,
                    occurred_at,
                }
            } else if event_type.contains("cancel") || event_type.contains("delete") {
                ConnectorEvent::DocumentDeleted {
                    document_id: id,
                    occurred_at,
                }
            } else {
                ConnectorEvent::DocumentUpdated {
                    document_id: id,
                    occurred_at,
                }
            };
            events.push(event);
        }
        Ok(events)
    }
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct CdiscountWebhookEvent {
    #[serde(default, alias = "OrderNumber", alias = "orderNumber")]
    order_id: serde_json::Value,
    #[serde(default, alias = "eventType")]
    event: String,
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
                "unused",
                "unused",
                Utc::now() + Duration::hours(1),
                "orders",
            ))
        }
    }

    fn oauth() -> Arc<dyn OAuth2CodeExchange> {
        Arc::new(FixedOAuth)
    }

    fn cfg() -> ConnectorConfig {
        ConnectorConfig::new(
            ConnectorKind::Cdiscount,
            AuthKind::ApiKey,
            ScopeId::new_v4(),
        )
        .with_auth_config(serde_json::json!({
            "api_key": "seller-token",
            "api_base_url": "https://api.test/cdiscount",
            "webhook_secret": "cdiscount-secret",
        }))
    }

    fn cfg_oauth() -> ConnectorConfig {
        ConnectorConfig::new(
            ConnectorKind::Cdiscount,
            AuthKind::OAuth2,
            ScopeId::new_v4(),
        )
        .with_auth_config(serde_json::json!({
            "authorization_code": "auth-code",
            "api_base_url": "https://api.test/cdiscount",
            "webhook_secret": "cdiscount-secret",
        }))
    }

    /// A real-shaped `GetOrderListResponse` SOAP envelope (element local
    /// names match the WSDL; WCF emits namespace-prefixed tags).
    fn order_list_soap(orders: &[(&str, &str, &str, &str)]) -> Vec<u8> {
        use std::fmt::Write as _;
        let mut body = String::new();
        for (num, state, created, modified) in orders {
            let _ = write!(
                body,
                "<a:Order><a:OrderNumber>{num}</a:OrderNumber>\
<a:OrderState>{state}</a:OrderState>\
<a:CreationDate>{created}</a:CreationDate>\
<a:ModifiedDate>{modified}</a:ModifiedDate></a:Order>"
            );
        }
        format!(
            "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\">\
<s:Body><GetOrderListResponse xmlns=\"http://www.cdiscount.com\">\
<GetOrderListResult xmlns:a=\"http://www.cdiscount.com\">\
<a:OrderList>{body}</a:OrderList></GetOrderListResult>\
</GetOrderListResponse></s:Body></s:Envelope>"
        )
        .into_bytes()
    }

    fn soap_ok(orders: &[(&str, &str, &str, &str)]) -> MockResponse {
        MockResponse::ok_json(order_list_soap(orders))
    }

    #[test]
    fn authenticate_reads_api_key() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = CdiscountConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let token = c.authenticate(&cfg()).unwrap();
        assert_eq!(token.access_token.expose(), "seller-token");
        assert!(token.refresh_token.is_none());
        assert_eq!(token.token_type, API_KEY_TOKEN_TYPE);
    }

    #[test]
    fn authenticate_falls_back_to_oauth_code() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = CdiscountConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let token = c.authenticate(&cfg_oauth()).unwrap();
        assert_eq!(token.access_token.expose(), "unused");
    }

    #[test]
    fn authenticate_requires_credential() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = CdiscountConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let bare = ConnectorConfig::new(
            ConnectorKind::Cdiscount,
            AuthKind::ApiKey,
            ScopeId::new_v4(),
        );
        assert!(matches!(
            c.authenticate(&bare),
            Err(ConnectorError::Auth(_))
        ));
    }

    #[test]
    fn initial_sync_posts_soap_with_token_and_parses_orders() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            "https://api.test/cdiscount",
            soap_ok(&[
                (
                    "ORDER-1",
                    "AcceptedBySeller",
                    "2024-01-01T00:00:00+00:00",
                    "2024-01-02T00:00:00+00:00",
                ),
                (
                    "ORDER-2",
                    "Shipped",
                    "2024-01-03T00:00:00+00:00",
                    "2024-01-04T00:00:00+00:00",
                ),
            ]),
        );
        let c = CdiscountConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let res = c.initial_sync(&cfg(), &tok).unwrap();
        assert_eq!(res.events.len(), 2);
        // Watermark is the latest CreationDate, tagged with the boundary id.
        assert_eq!(
            res.next_cursor.as_deref(),
            Some("2024-01-03T00:00:00+00:00|ORDER-2")
        );
        let recorded = transport.recorded();
        // SOAP request: POST, correct SOAPAction, token in the header
        // message — not an X-Cdiscount-Api-Key HTTP header.
        assert_eq!(recorded[0].method, HttpMethod::Post);
        assert!(recorded[0]
            .headers
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("SOAPAction") && v.contains("GetOrderList")));
        assert!(!recorded[0]
            .headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("X-Cdiscount-Api-Key")));
        let body = String::from_utf8(recorded[0].body.clone()).unwrap();
        assert!(body.contains("<msg:TokenId>seller-token</msg:TokenId>"));
        assert!(body.contains("GetOrderList"));
    }

    #[test]
    fn incremental_sync_filters_begin_creation_date_and_dedups() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            "https://api.test/cdiscount",
            soap_ok(&[
                // Boundary order already emitted at the cursor instant — must
                // be deduped.
                (
                    "ORDER-1",
                    "Shipped",
                    "2024-01-03T00:00:00+00:00",
                    "2024-01-03T00:00:00+00:00",
                ),
                // Brand-new order sharing the exact boundary instant — must
                // still be surfaced (the old bare-timestamp cursor dropped it).
                (
                    "ORDER-3",
                    "AcceptedBySeller",
                    "2024-01-03T00:00:00+00:00",
                    "2024-01-03T00:00:00+00:00",
                ),
                // Newer order — must be emitted.
                (
                    "ORDER-2",
                    "AcceptedBySeller",
                    "2024-01-05T00:00:00+00:00",
                    "2024-01-05T00:00:00+00:00",
                ),
            ]),
        );
        let c = CdiscountConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let mut state = SyncState::new(c.instance);
        state.cursor = Some("2024-01-03T00:00:00+00:00|ORDER-1".to_string());
        let res = c.incremental_sync(&cfg(), &tok, &state).unwrap();
        let ids: Vec<&str> = res
            .events
            .iter()
            .map(|e| e.document_id().as_str())
            .collect();
        assert_eq!(ids, vec!["ORDER-3", "ORDER-2"]);
        assert_eq!(
            res.next_cursor.as_deref(),
            Some("2024-01-05T00:00:00+00:00|ORDER-2")
        );
        // The filter carried the BeginCreationDate.
        let body = String::from_utf8(transport.recorded()[0].body.clone()).unwrap();
        assert!(body
            .contains("<tns:BeginCreationDate>2024-01-03T00:00:00+00:00</tns:BeginCreationDate>"));
    }

    #[test]
    fn fetch_content_filters_by_order_reference() {
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            "https://api.test/cdiscount",
            soap_ok(&[(
                "ORDER-9",
                "Shipped",
                "2024-01-01T00:00:00+00:00",
                "2024-01-02T00:00:00+00:00",
            )]),
        );
        let c = CdiscountConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let content = c
            .fetch_content(&cfg(), &tok, &SourceDocumentId::new("ORDER-9"))
            .unwrap();
        let text = String::from_utf8(content.body).unwrap();
        assert!(text.contains("Cdiscount order ORDER-9"));
        assert!(text.contains("Shipped"));
        let body = String::from_utf8(transport.recorded()[0].body.clone()).unwrap();
        assert!(body.contains("<arr:string>ORDER-9</arr:string>"));
    }

    #[test]
    fn subscribe_webhook_is_polling_only() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = CdiscountConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let tok = c.authenticate(&cfg()).unwrap();
        let sub = c
            .subscribe_webhook(&cfg(), &tok, "https://callback.test/cdiscount")
            .unwrap();
        assert_eq!(sub.callback_url, "https://callback.test/cdiscount");
        assert!(sub.provider_subscription_id.is_none());
    }

    #[test]
    fn handle_webhook_event_parses_order_number() {
        let transport = Arc::new(MockHttpTransport::new());
        let c = CdiscountConnector::new(ConnectorInstanceId::new_v4(), transport, oauth());
        let body = serde_json::to_vec(&serde_json::json!({
            "OrderNumber": "ORDER-5",
            "eventType": "order.created"
        }))
        .unwrap();
        let events = c.handle_webhook_event(&body).unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ConnectorEvent::DocumentCreated { document_id, .. } => {
                assert_eq!(document_id.as_str(), "ORDER-5");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn parse_orders_handles_namespaced_and_missing_fields() {
        // Prefix-agnostic parsing; an order with no number is skipped.
        let xml = "<x:OrderList><x:Order><x:OrderNumber>A1</x:OrderNumber>\
<x:CreationDate>2024-01-01T00:00:00+00:00</x:CreationDate></x:Order>\
<x:Order><x:OrderState>Shipped</x:OrderState></x:Order></x:OrderList>";
        let orders = parse_orders(xml);
        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].order_number, "A1");
        assert!(orders[0].order_state.is_none());
    }

    #[test]
    fn parse_orders_is_depth_aware_for_nested_same_name_elements() {
        // An <Order> whose body contains a nested element sharing the
        // `Order` local name must not be truncated at the inner close.
        // A depth-agnostic parser would stop the outer block at the
        // first </Order>, dropping the trailing CreationDate and
        // mis-framing the following top-level order.
        let xml = "<OrderList>\
<Order><OrderNumber>OUTER</OrderNumber>\
<SubOrders><Order><OrderNumber>INNER</OrderNumber></Order></SubOrders>\
<CreationDate>2024-01-01T00:00:00+00:00</CreationDate></Order>\
<Order><OrderNumber>NEXT</OrderNumber>\
<CreationDate>2024-02-02T00:00:00+00:00</CreationDate></Order>\
</OrderList>";
        let orders = parse_orders(xml);
        // Only the two top-level orders are emitted; the nested one is
        // contained within OUTER's block, not surfaced separately.
        assert_eq!(orders.len(), 2);
        assert_eq!(orders[0].order_number, "OUTER");
        // The trailing CreationDate survives — proof the outer block was
        // not cut off at the inner </Order>.
        assert_eq!(
            orders[0].creation_date.as_deref(),
            Some("2024-01-01T00:00:00+00:00")
        );
        assert_eq!(orders[1].order_number, "NEXT");
        assert_eq!(
            orders[1].creation_date.as_deref(),
            Some("2024-02-02T00:00:00+00:00")
        );
    }

    #[test]
    fn production_base_url_targets_soap_service() {
        // Exercises the real DEFAULT_API_BASE_URL (the circular tests
        // override it). The production target is the SOAP/WCF endpoint,
        // not the non-resolving api.cdiscount.com host.
        let transport = Arc::new(MockHttpTransport::new());
        transport.expect(
            HttpMethod::Post,
            "https://wsvc.cdiscount.com/MarketplaceAPIService.svc",
            soap_ok(&[(
                "ORDER-1",
                "Shipped",
                "2024-01-01T00:00:00+00:00",
                "2024-01-02T00:00:00+00:00",
            )]),
        );
        let prod_cfg = ConnectorConfig::new(
            ConnectorKind::Cdiscount,
            AuthKind::ApiKey,
            ScopeId::new_v4(),
        )
        .with_auth_config(serde_json::json!({ "api_key": "seller-token" }));
        let c = CdiscountConnector::new(ConnectorInstanceId::new_v4(), transport.clone(), oauth());
        let tok = c.authenticate(&prod_cfg).unwrap();
        let res = c.initial_sync(&prod_cfg, &tok).unwrap();
        assert_eq!(res.events.len(), 1);
        assert_eq!(
            transport.recorded()[0].url,
            "https://wsvc.cdiscount.com/MarketplaceAPIService.svc"
        );
    }
}
