//! Connector configuration and runtime instance.

use chrono::Duration;
use evidence_store::ScopeId;
use serde::{Deserialize, Serialize};

use crate::sync::SyncState;
use crate::token_vault::ConnectorInstanceId;

/// The well-known source kinds the connector framework supports
/// (per `docs/technical/design.md` §10.2). Kept as an enum so attachments and
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
    /// Intuit QuickBooks Online.
    QuickBooks,
    /// Xero accounting.
    Xero,
    /// Stripe payments.
    Stripe,
    /// Shopify storefront / admin.
    Shopify,
    /// Airtable bases.
    Airtable,
    /// GitLab projects / issues.
    GitLab,
    /// Bitbucket repositories / pull requests.
    Bitbucket,
    /// Trello boards / cards.
    Trello,
    /// Miro boards.
    Miro,
    /// DocuSign envelopes.
    DocuSign,
    /// Dropbox — cloud file storage (API v2).
    Dropbox,
    /// Box — cloud content management (Content API 2.0).
    Box,
    /// Microsoft SharePoint — document libraries via Graph drive delta.
    SharePoint,
    /// Microsoft Teams — channel messages via Graph.
    Teams,
    /// Discord — channel messages via the REST API.
    Discord,
    /// Zoom — cloud recordings / meetings (REST v2).
    Zoom,
    /// Google Calendar — events via Calendar API v3.
    GoogleCalendar,
    /// Google Docs — documents via Docs API v1 + Drive change feed.
    GoogleDocs,
    /// Google Sheets — spreadsheets via Sheets API v4 + Drive change feed.
    GoogleSheets,
    /// Google Meet — conference records / transcripts via Meet REST API.
    GoogleMeet,
    /// Salesforce CRM.
    Salesforce,
    /// ServiceNow ITSM (Table API).
    ServiceNow,
    /// Zendesk Support.
    Zendesk,
    /// Linear issue tracker.
    Linear,
    /// Asana work management.
    Asana,
    /// Monday.com work OS.
    Monday,
    /// ClickUp work management.
    ClickUp,
    /// Freshdesk support.
    Freshdesk,
    /// Intercom messaging / support.
    Intercom,
    /// Pipedrive CRM.
    Pipedrive,
    // Singapore/Thailand/SEA connectors
    /// LINE Messaging API — messages, groups, rich menus.
    Line,
    /// Grab for Business API — orders, drivers, analytics.
    Grab,
    /// GoTo / Gojek Partner API — orders, merchant data.
    Gojek,
    /// Talenox API — Singapore HR/payroll (employees, payroll, leave).
    Talenox,
    /// Odoo REST API — ERP invoices, CRM, inventory (SEA SMEs).
    OdooSea,
    /// Fastwork API — Thai freelance marketplace (projects, contracts, payments).
    Fastwork,
    /// TrueMoney Business API — Thai e-wallet transaction records / analytics.
    TrueMoney,
    /// SCB Easy Business API — Siam Commercial Bank account transactions / transfers.
    ScbEasy,
    /// PromptPay QR reconciliation API — Thai national payment rail.
    PromptPay,
    /// Tokopedia Seller API — Indonesia marketplace (orders, products, chat).
    Tokopedia,
    // Vietnam connectors (WS5).
    /// Zalo Official Account — Vietnam's dominant messaging platform.
    Zalo,
    /// VNPay merchant gateway — #1 Vietnamese payment gateway.
    VNPay,
    /// MoMo Business — leading Vietnamese e-wallet.
    MoMo,
    /// Tiki Seller Center — top Vietnamese e-commerce marketplace.
    Tiki,
    /// Shopee Open Platform (Vietnam) — e-commerce marketplace.
    ShopeeVN,
    /// Lazada Open Platform (Vietnam) — e-commerce marketplace.
    LazadaVN,
    /// Viettel Post — Vietnam's largest logistics carrier.
    ViettelPost,
    /// KiotViet — #1 Vietnamese POS / retail SaaS.
    KiotViet,
    /// Sapo — Vietnamese e-commerce / POS platform.
    Sapo,
    /// Base.vn — Vietnamese enterprise collaboration suite (HR, CRM, project).
    BaseVN,
    // GCC / Middle East connectors
    /// Careem Business — UAE super-app (orders / drivers / analytics).
    Careem,
    /// Talabat Partner — GCC food delivery (orders / restaurants).
    Talabat,
    /// Noon Seller Center — UAE/Saudi e-commerce (orders / products).
    Noon,
    /// Amazon.ae — UAE marketplace via Amazon Selling-Partner API.
    AmazonAE,
    /// Tabby Merchant — UAE/Saudi BNPL (payments / settlements).
    Tabby,
    /// Foodics — Saudi/GCC restaurant management (POS / menu).
    Foodics,
    /// Zoho CRM/Books — GCC SME CRM (contacts / deals / invoices).
    Zoho,
    /// Bayt.com — Middle East job board (postings / applications).
    Bayt,
    /// Fetchr — UAE last-mile logistics (shipments / tracking).
    Fetchr,
    /// Amazon Payment Services (PayFort) — payments / settlements.
    Payfort,
    // UK connectors (WS8)
    /// Monzo Business — UK Open Banking (transactions, pots).
    MonzoBusiness,
    /// Revolut Business — UK business API (transactions, counterparties).
    RevolutBusiness,
    /// FreeAgent — UK accounting (invoices, contacts, projects).
    FreeAgent,
    /// GoCardless — UK Direct Debit (mandates, payments, payouts).
    GoCardless,
    /// Royal Mail — UK shipping API (tracking, shipments).
    RoyalMail,
    /// Deliveroo — UK restaurant partner API (orders, menu).
    Deliveroo,
    /// Just Eat — UK partner API (orders, restaurant data).
    JustEat,
    /// Companies House — UK gov API (company search, filings).
    CompaniesHouse,
    /// HMRC Making Tax Digital — VAT returns, obligations.
    HmrcMtd,
    /// Starling Bank — UK business API (transactions, spaces).
    Starling,
    // Germany connectors (WS9)
    /// N26 Business — German banking API (transactions).
    N26Business,
    /// DATEV — German accounting interface (bookings, documents).
    Datev,
    /// lexoffice — German accounting API (invoices, contacts, vouchers).
    Lexoffice,
    /// DHL Business — German shipping API (shipments, tracking).
    DhlBusiness,
    /// Otto — German marketplace partner API (orders, products).
    Otto,
    /// Zalando — German marketplace (ZMS) API (orders, articles).
    Zalando,
    /// Deutsche Post — German Warenpost API (shipments).
    DeutschePost,
    /// Personio — German HR API (employees, absences, documents).
    Personio,
    /// sevDesk — German accounting API (invoices, contacts).
    SevDesk,
    /// Billomat — German invoicing API (invoices, clients).
    Billomat,
    // France connectors (WS10)
    /// Qonto — French business banking API (transactions, labels).
    Qonto,
    /// Pennylane — French accounting API (invoices, suppliers).
    Pennylane,
    /// PayFit — French HR/payroll API (employees, payslips).
    PayFit,
    /// Colissimo — French La Poste API (parcels, tracking).
    Colissimo,
    /// Cdiscount — French marketplace API (orders, products).
    Cdiscount,
    /// MangoPay — French payment API (wallets, payins/payouts).
    MangoPay,
    /// Brevo (Sendinblue) — French marketing API (contacts, campaigns).
    Sendinblue,
    /// OVHcloud — French cloud API (services, billing, tickets).
    OvhCloud,
    /// Alan — French health insurance API (members, contracts).
    Alan,
    /// Swile — French benefits API (transactions, meal vouchers).
    Swile,
    // Switzerland connectors (WS11)
    /// PostFinance — Swiss e-finance API (transactions).
    PostFinance,
    /// TWINT — Swiss merchant API (payments, refunds).
    Twint,
    /// Swiss Post — Swiss API (parcels, tracking).
    SwissPost,
    /// Bexio — Swiss ERP API (invoices, contacts, orders).
    Bexio,
    /// Abacus — Swiss ERP API (accounting, HR).
    Abacus,
    /// Ricardo — Swiss marketplace API (listings, orders).
    Ricardo,
    /// Digitec Galaxus — Swiss marketplace API (orders).
    DigitecGalaxus,
    /// SIX Payment Services — Swiss payments API (transactions).
    SixPayment,
    /// Klara — Swiss business admin API (invoices, CRM).
    Klara,
    /// Beem — Swiss banking API (accounts, transactions).
    Beem,
    // Australia connectors (WS12)
    /// MYOB — Australian AccountRight API (invoices, contacts, journals).
    Myob,
    /// Afterpay — Australian BNPL merchant API (orders, payments, refunds).
    Afterpay,
    /// Australia Post — Australian shipping API (shipments, tracking).
    AustraliaPost,
    /// Employment Hero — Australian HR API (employees, leave, payroll).
    EmploymentHero,
    /// Deputy — Australian workforce API (rosters, timesheets).
    Deputy,
    /// Tyro — Australian payments API (transactions, settlements).
    Tyro,
    /// Prospa — Australian business lending API (applications, repayments).
    Prospa,
    /// SEEK — Australian advertiser API (job ads, applications).
    Seek,
    /// Campaign Monitor — Australian email API (subscribers, campaigns).
    CampaignMonitor,
    /// Pinch Payments — Australian direct debit API (payments).
    Pinch,
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
            Self::QuickBooks => "quickbooks",
            Self::Xero => "xero",
            Self::Stripe => "stripe",
            Self::Shopify => "shopify",
            Self::Airtable => "airtable",
            Self::GitLab => "gitlab",
            Self::Bitbucket => "bitbucket",
            Self::Trello => "trello",
            Self::Miro => "miro",
            Self::DocuSign => "docusign",
            Self::Dropbox => "dropbox",
            Self::Box => "box",
            Self::SharePoint => "sharepoint",
            Self::Teams => "teams",
            Self::Discord => "discord",
            Self::Zoom => "zoom",
            Self::GoogleCalendar => "google_calendar",
            Self::GoogleDocs => "google_docs",
            Self::GoogleSheets => "google_sheets",
            Self::GoogleMeet => "google_meet",
            Self::Salesforce => "salesforce",
            Self::ServiceNow => "servicenow",
            Self::Zendesk => "zendesk",
            Self::Linear => "linear",
            Self::Asana => "asana",
            Self::Monday => "monday",
            Self::ClickUp => "clickup",
            Self::Freshdesk => "freshdesk",
            Self::Intercom => "intercom",
            Self::Pipedrive => "pipedrive",
            // Singapore/Thailand/SEA connectors
            Self::Line => "line",
            Self::Grab => "grab",
            Self::Gojek => "gojek",
            Self::Talenox => "talenox",
            Self::OdooSea => "odoo_sea",
            Self::Fastwork => "fastwork",
            Self::TrueMoney => "true_money",
            Self::ScbEasy => "scb_easy",
            Self::PromptPay => "promptpay",
            Self::Tokopedia => "tokopedia",
            // Vietnam connectors (WS5).
            Self::Zalo => "zalo",
            Self::VNPay => "vnpay",
            Self::MoMo => "momo",
            Self::Tiki => "tiki",
            Self::ShopeeVN => "shopee_vn",
            Self::LazadaVN => "lazada_vn",
            Self::ViettelPost => "viettel_post",
            Self::KiotViet => "kiotviet",
            Self::Sapo => "sapo",
            Self::BaseVN => "base_vn",
            // GCC / Middle East connectors
            Self::Careem => "careem",
            Self::Talabat => "talabat",
            Self::Noon => "noon",
            Self::AmazonAE => "amazon_ae",
            Self::Tabby => "tabby",
            Self::Foodics => "foodics",
            Self::Zoho => "zoho",
            Self::Bayt => "bayt",
            Self::Fetchr => "fetchr",
            Self::Payfort => "payfort",
            Self::MonzoBusiness => "monzo_business",
            Self::RevolutBusiness => "revolut_business",
            Self::FreeAgent => "freeagent",
            Self::GoCardless => "go_cardless",
            Self::RoyalMail => "royal_mail",
            Self::Deliveroo => "deliveroo",
            Self::JustEat => "just_eat",
            Self::CompaniesHouse => "companies_house",
            Self::HmrcMtd => "hmrc_mtd",
            Self::Starling => "starling",
            Self::N26Business => "n26_business",
            Self::Datev => "datev",
            Self::Lexoffice => "lexoffice",
            Self::DhlBusiness => "dhl_business",
            Self::Otto => "otto",
            Self::Zalando => "zalando",
            Self::DeutschePost => "deutsche_post",
            Self::Personio => "personio",
            Self::SevDesk => "sev_desk",
            Self::Billomat => "billomat",
            Self::Qonto => "qonto",
            Self::Pennylane => "pennylane",
            Self::PayFit => "payfit",
            Self::Colissimo => "colissimo",
            Self::Cdiscount => "cdiscount",
            Self::MangoPay => "mangopay",
            Self::Sendinblue => "sendinblue",
            Self::OvhCloud => "ovh_cloud",
            Self::Alan => "alan",
            Self::Swile => "swile",
            Self::PostFinance => "postfinance",
            Self::Twint => "twint",
            Self::SwissPost => "swiss_post",
            Self::Bexio => "bexio",
            Self::Abacus => "abacus",
            Self::Ricardo => "ricardo",
            Self::DigitecGalaxus => "digitec_galaxus",
            Self::SixPayment => "six_payment",
            Self::Klara => "klara",
            Self::Beem => "beem",
            Self::Myob => "myob",
            Self::Afterpay => "afterpay",
            Self::AustraliaPost => "australia_post",
            Self::EmploymentHero => "employment_hero",
            Self::Deputy => "deputy",
            Self::Tyro => "tyro",
            Self::Prospa => "prospa",
            Self::Seek => "seek",
            Self::CampaignMonitor => "campaign_monitor",
            Self::Pinch => "pinch",
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
    /// migration.
    ///
    /// **Secret values belong in the host's keychain, not this
    /// field.** Production hosts MUST surface OAuth2 `client_secret`
    /// values through a registered
    /// [`ClientSecretResolver`](crate::oauth::ClientSecretResolver)
    /// (see [`crate::oauth::OAuth2Client::set_resolver`]). The
    /// framework reads `auth_config_json["client_secret"]` as a
    /// fallback only when the resolver is unset OR returns `None`
    /// — that fallback exists strictly so test harnesses, single-
    /// tenant CLI hosts, and migration scripts can stand up the
    /// OAuth2 round-trip without the resolver FFI ceremony. The
    /// fallback secret does live on disk (encrypted under the per-
    /// scope DEK in the substrate's SQLCipher store), which matters
    /// for at-rest theft and backup-snapshot exposure scenarios.
    /// Treat its presence here as a deliberate test-or-dev choice,
    /// not a production pattern.
    ///
    /// Long-lived API keys (e.g. HubSpot private app tokens) follow
    /// the same rule — surface them through the host's keychain via
    /// the resolver (extension point pending), not via this field.
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
