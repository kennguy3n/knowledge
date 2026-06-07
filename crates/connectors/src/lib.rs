//! `connectors` — vendor-specific [`Connector`](connector_framework::Connector)
//! implementations for the Knowledge substrate.
//!
//! Per `docs/technical/design.md` §10.2 and `docs/technical/architecture.md` §4.1, the substrate
//! ingests evidence from external systems through the
//! [`connector_framework`] trait. This crate ships sixty concrete
//! connectors against the most common B2B sources:
//!
//! * [`google_drive::GoogleDriveConnector`] — Google Drive API v3
//!   (`files.list`, Changes API, push notifications).
//! * [`onedrive::OneDriveConnector`] — Microsoft Graph (`/drive/root/delta`,
//!   Graph subscriptions).
//! * [`notion::NotionConnector`] — Notion API (`/search`, `/databases`,
//!   polled incremental — Notion has no native webhooks).
//! * [`jira::JiraConnector`] — Jira REST v3 (JQL `/search`, issue webhooks).
//! * [`confluence::ConfluenceConnector`] — Confluence REST (`/content`,
//!   page/space webhooks).
//! * [`figma::FigmaConnector`] — Figma REST (`/files/{key}`,
//!   `/files/{key}/components`, file-update webhooks).
//! * [`hubspot::HubSpotConnector`] — HubSpot CRM v3
//!   (`/crm/v3/objects/{type}`, webhook subscriptions).
//! * [`slack::SlackConnector`] — Slack Web API (`conversations.list` /
//!   `conversations.history`) plus the Events API for `message`,
//!   `file_shared`, and `channel_archive` push events.
//! * [`email::EmailConnector`] — Gmail API (`messages.list` + Cloud
//!   Pub/Sub push) and Microsoft Graph (`/me/messages` +
//!   `/subscriptions`) under a shared `EmailProvider` enum.
//! * [`github::GitHubConnector`] — GitHub REST API v3
//!   (`/repos/{owner}/{repo}/issues`, repository webhooks).
//! * [`salesforce::SalesforceConnector`] — Salesforce REST API v59
//!   (SOQL `/query`, sObjects).
//! * [`servicenow::ServiceNowConnector`] — ServiceNow Table API
//!   (`/api/now/table/{table}`).
//! * [`zendesk::ZendeskConnector`] — Zendesk Support API
//!   (`/api/v2/tickets`, incremental exports).
//! * [`linear::LinearConnector`] — Linear GraphQL API (`/graphql`).
//! * [`asana::AsanaConnector`] — Asana REST (`/api/1.0/tasks`,
//!   `/projects`, webhooks).
//! * [`monday::MondayConnector`] — Monday.com GraphQL (`/v2`).
//! * [`clickup::ClickUpConnector`] — ClickUp REST v2
//!   (`/api/v2/team/.../task`, webhooks).
//! * [`freshdesk::FreshdeskConnector`] — Freshdesk REST v2
//!   (`/api/v2/tickets`).
//! * [`intercom::IntercomConnector`] — Intercom REST (`/conversations`,
//!   `/contacts`, webhooks).
//! * [`pipedrive::PipedriveConnector`] — Pipedrive REST v1
//!   (`/v1/deals`, `/v1/persons`, webhooks).
//!
//! Batch 3 adds ten business & developer-tool sources:
//!
//! * [`quickbooks::QuickBooksConnector`] — QuickBooks Online Accounting
//!   API (`/v3/company/{id}/query`, portal-configured webhooks).
//! * [`xero::XeroConnector`] — Xero Accounting API (`/api.xro/2.0/...`,
//!   `If-Modified-Since` incremental, portal-configured webhooks).
//! * [`stripe::StripeConnector`] — Stripe REST (`/v1/customers`,
//!   `starting_after` pagination, `/v1/webhook_endpoints`).
//! * [`shopify::ShopifyConnector`] — Shopify Admin REST
//!   (`/admin/api/.../orders.json`, `since_id` pagination, webhooks).
//! * [`airtable::AirtableConnector`] — Airtable REST (`/v0/{baseId}/{table}`,
//!   `offset` pagination, base webhooks).
//! * [`gitlab::GitLabConnector`] — GitLab REST v4 (`/projects/{id}/issues`,
//!   `updated_after` incremental, project hooks).
//! * [`bitbucket::BitbucketConnector`] — Bitbucket REST 2.0
//!   (`/repositories/{ws}/{repo}/pullrequests`, repo webhooks).
//! * [`trello::TrelloConnector`] — Trello REST (`/1/boards/{id}/cards`,
//!   key+token auth, webhooks).
//! * [`miro::MiroConnector`] — Miro REST v2 (`/v2/boards`, `/items`,
//!   board-subscription webhooks).
//! * [`docusign::DocuSignConnector`] — DocuSign eSignature REST
//!   (`/restapi/v2.1/accounts/{id}/envelopes`, Connect webhooks).
//!
//! The Singapore/Thailand/SEA batch adds ten regional sources:
//!
//! * [`line::LineConnector`] — LINE Messaging API (rich menus +
//!   webhook-delivered messages).
//! * [`grab::GrabConnector`] — Grab for Business API (orders,
//!   `page_size`/`page_index` pagination, OAuth2).
//! * [`gojek::GojekConnector`] — GoTo/Gojek Partner API (orders,
//!   API-key header auth).
//! * [`talenox::TalenoxConnector`] — Talenox HR/payroll (employees,
//!   API-key bearer auth).
//! * [`odoo_sea::OdooSeaConnector`] — Odoo REST (invoices, session-id
//!   header auth).
//! * [`fastwork::FastworkConnector`] — Fastwork freelance marketplace
//!   (projects, OAuth2).
//! * [`true_money::TrueMoneyConnector`] — TrueMoney Business
//!   (transactions, API key + HMAC-SHA256 request signing).
//! * [`scb_easy::ScbEasyConnector`] — SCB Easy Open Banking (account
//!   transactions, OAuth2).
//! * [`promptpay::PromptPayConnector`] — PromptPay QR reconciliation
//!   (settlements, API-key auth).
//! * [`tokopedia::TokopediaConnector`] — Tokopedia Seller API (orders,
//!   OAuth2).
//!
//! Each connector models the vendor's REST contract as plain serde
//! types and issues real HTTP requests through an injected
//! [`connector_framework::HttpTransport`] — production wires the
//! `reqwest`-backed [`connector_framework::BlockingHttpTransport`]
//! (3 retries with exponential backoff, `Retry-After` honoured,
//! 30s default timeout) while tests wire
//! [`connector_framework::MockHttpTransport`] with deterministic
//! canned responses. The `Connector` trait stays synchronous so the
//! call surface can be exhaustively unit-tested, but every code path
//! that crosses the trait boundary in production goes over the wire.
//! OAuth2 token exchange runs through the
//! [`connector_framework::OAuth2CodeExchange`] trait, which the
//! production binary wires to
//! [`connector_framework::OAuth2Client`] (also `reqwest`-backed).

#![deny(missing_docs)]

// Crate-internal helpers shared by the connectors' `fetch_content`
// implementations (raw binary GET, base64 decode, HTML / ADF
// flattening). Not part of the public API.
mod content;

// Crate-internal helper for timestamp-keyed incremental cursors that
// also remember the ids emitted at the exact boundary instant (Zoom,
// Google Meet). Not part of the public API.
mod timestamp_cursor;

// Crate-internal HMAC-SHA256 request-signing helper shared by the
// Vietnam marketplace connectors (Tiki, Shopee, Lazada). Not part of
// the public API.
mod sign;

// Crate-internal request-signing primitives (HMAC-SHA256, SHA-256,
// AWS Signature v4) shared by the GCC / Middle East connectors whose
// auth goes beyond a bearer token (Noon, PayFort, Amazon.ae). Not part
// of the public API.
mod signing;

// STABLE
pub mod airtable;
// STABLE
pub mod asana;
// STABLE
pub mod bitbucket;
// STABLE
pub mod box_connector;
// STABLE
pub mod clickup;
// STABLE
pub mod confluence;
// STABLE
pub mod discord;
// STABLE
pub mod docusign;
// STABLE
pub mod dropbox;
// STABLE
pub mod email;
// STABLE
pub mod figma;
// STABLE
pub mod freshdesk;
// STABLE
pub mod github;
// STABLE
pub mod gitlab;
// STABLE
pub mod google_calendar;
// STABLE
pub mod google_docs;
// STABLE
pub mod google_drive;
// STABLE
pub mod google_meet;
// STABLE
pub mod google_sheets;
// STABLE
pub mod hubspot;
// STABLE
pub mod intercom;
// STABLE
pub mod jira;
// STABLE
pub mod linear;
// STABLE
pub mod miro;
// STABLE
pub mod monday;
// STABLE
pub mod notion;
// STABLE
pub mod onedrive;
// STABLE
pub mod pipedrive;
// STABLE
pub mod quickbooks;
// STABLE
pub mod salesforce;
// STABLE
pub mod servicenow;
// STABLE
pub mod sharepoint;
// STABLE
pub mod shopify;
// STABLE
pub mod slack;
// STABLE
pub mod stripe;
// STABLE
pub mod teams;
// STABLE
pub mod trello;
// STABLE
pub mod xero;
// STABLE
pub mod zendesk;
// STABLE
pub mod zoom;

// Singapore/Thailand/SEA connectors
// STABLE
pub mod fastwork;
// STABLE
pub mod gojek;
// STABLE
pub mod grab;
// STABLE
pub mod line;
// STABLE
pub mod odoo_sea;
// STABLE
pub mod promptpay;
// STABLE
pub mod scb_easy;
// STABLE
pub mod talenox;
// STABLE
pub mod tokopedia;
// STABLE
pub mod true_money;

// Vietnam connectors (WS5) — Asia market expansion.
// STABLE
pub mod base_vn;
// STABLE
pub mod kiotviet;
// STABLE
pub mod lazada_vn;
// STABLE
pub mod momo;
// STABLE
pub mod sapo;
// STABLE
pub mod shopee_vn;
// STABLE
pub mod tiki;
// STABLE
pub mod viettel_post;
// STABLE
pub mod vnpay;
// STABLE
pub mod zalo;

// GCC / Middle East connectors
// STABLE
pub mod amazon_ae;
// STABLE
pub mod bayt;
// STABLE
pub mod careem;
// STABLE
pub mod fetchr;
// STABLE
pub mod foodics;
// STABLE
pub mod noon;
// STABLE
pub mod payfort;
// STABLE
pub mod tabby;
// STABLE
pub mod talabat;
// STABLE
pub mod zoho_me;

// STABLE
pub use airtable::AirtableConnector;
// STABLE
pub use asana::AsanaConnector;
// STABLE
pub use bitbucket::BitbucketConnector;
// STABLE
pub use box_connector::BoxConnector;
// STABLE
pub use clickup::ClickUpConnector;
// STABLE
pub use confluence::ConfluenceConnector;
// STABLE
pub use discord::DiscordConnector;
// STABLE
pub use docusign::DocuSignConnector;
// STABLE
pub use dropbox::DropboxConnector;
// STABLE
pub use email::EmailConnector;
// STABLE
pub use figma::FigmaConnector;
// STABLE
pub use freshdesk::FreshdeskConnector;
// STABLE
pub use github::GitHubConnector;
// STABLE
pub use gitlab::GitLabConnector;
// STABLE
pub use google_calendar::GoogleCalendarConnector;
// STABLE
pub use google_docs::GoogleDocsConnector;
// STABLE
pub use google_drive::GoogleDriveConnector;
// STABLE
pub use google_meet::GoogleMeetConnector;
// STABLE
pub use google_sheets::GoogleSheetsConnector;
// STABLE
pub use hubspot::HubSpotConnector;
// STABLE
pub use intercom::IntercomConnector;
// STABLE
pub use jira::JiraConnector;
// STABLE
pub use linear::LinearConnector;
// STABLE
pub use miro::MiroConnector;
// STABLE
pub use monday::MondayConnector;
// STABLE
pub use notion::NotionConnector;
// STABLE
pub use onedrive::OneDriveConnector;
// STABLE
pub use pipedrive::PipedriveConnector;
// STABLE
pub use quickbooks::QuickBooksConnector;
// STABLE
pub use salesforce::SalesforceConnector;
// STABLE
pub use servicenow::ServiceNowConnector;
// STABLE
pub use sharepoint::SharePointConnector;
// STABLE
pub use shopify::ShopifyConnector;
// STABLE
pub use slack::SlackConnector;
// STABLE
pub use stripe::StripeConnector;
// STABLE
pub use teams::TeamsConnector;
// STABLE
pub use trello::TrelloConnector;
// STABLE
pub use xero::XeroConnector;
// STABLE
pub use zendesk::ZendeskConnector;
// STABLE
pub use zoom::ZoomConnector;

// Singapore/Thailand/SEA connectors
// STABLE
pub use fastwork::FastworkConnector;
// STABLE
pub use gojek::GojekConnector;
// STABLE
pub use grab::GrabConnector;
// STABLE
pub use line::LineConnector;
// STABLE
pub use odoo_sea::OdooSeaConnector;
// STABLE
pub use promptpay::PromptPayConnector;
// STABLE
pub use scb_easy::ScbEasyConnector;
// STABLE
pub use talenox::TalenoxConnector;
// STABLE
pub use tokopedia::TokopediaConnector;
// STABLE
pub use true_money::TrueMoneyConnector;

// Vietnam connectors (WS5) — Asia market expansion.
// STABLE
pub use base_vn::BaseVNConnector;
// STABLE
pub use kiotviet::KiotVietConnector;
// STABLE
pub use lazada_vn::LazadaVNConnector;
// STABLE
pub use momo::MoMoConnector;
// STABLE
pub use sapo::SapoConnector;
// STABLE
pub use shopee_vn::ShopeeVNConnector;
// STABLE
pub use tiki::TikiConnector;
// STABLE
pub use viettel_post::ViettelPostConnector;
// STABLE
pub use vnpay::VNPayConnector;
// STABLE
pub use zalo::ZaloConnector;

// GCC / Middle East connectors
// STABLE
pub use amazon_ae::AmazonAeConnector;
// STABLE
pub use bayt::BaytConnector;
// STABLE
pub use careem::CareemConnector;
// STABLE
pub use fetchr::FetchrConnector;
// STABLE
pub use foodics::FoodicsConnector;
// STABLE
pub use noon::NoonConnector;
// STABLE
pub use payfort::PayfortConnector;
// STABLE
pub use tabby::TabbyConnector;
// STABLE
pub use talabat::TalabatConnector;
// STABLE
pub use zoho_me::ZohoConnector;

// UK connectors (WS8) — global market expansion.
// STABLE
pub mod monzo_business;
// STABLE
pub mod revolut_business;
// STABLE
pub mod freeagent;
// STABLE
pub mod go_cardless;
// STABLE
pub mod royal_mail;
// STABLE
pub mod deliveroo;
// STABLE
pub mod just_eat;
// STABLE
pub mod companies_house;
// STABLE
pub mod hmrc_mtd;
// STABLE
pub mod starling;
// STABLE
pub use monzo_business::MonzoBusinessConnector;
// STABLE
pub use revolut_business::RevolutBusinessConnector;
// STABLE
pub use freeagent::FreeAgentConnector;
// STABLE
pub use go_cardless::GoCardlessConnector;
// STABLE
pub use royal_mail::RoyalMailConnector;
// STABLE
pub use deliveroo::DeliverooConnector;
// STABLE
pub use just_eat::JustEatConnector;
// STABLE
pub use companies_house::CompaniesHouseConnector;
// STABLE
pub use hmrc_mtd::HmrcMtdConnector;
// STABLE
pub use starling::StarlingConnector;

// Germany connectors (WS9) — global market expansion.
// STABLE
pub mod n26_business;
// STABLE
pub mod datev;
// STABLE
pub mod lexoffice;
// STABLE
pub mod dhl_business;
// STABLE
pub mod otto;
// STABLE
pub mod zalando;
// STABLE
pub mod deutsche_post;
// STABLE
pub mod personio;
// STABLE
pub mod sev_desk;
// STABLE
pub mod billomat;
// STABLE
pub use n26_business::N26BusinessConnector;
// STABLE
pub use datev::DatevConnector;
// STABLE
pub use lexoffice::LexofficeConnector;
// STABLE
pub use dhl_business::DhlBusinessConnector;
// STABLE
pub use otto::OttoConnector;
// STABLE
pub use zalando::ZalandoConnector;
// STABLE
pub use deutsche_post::DeutschePostConnector;
// STABLE
pub use personio::PersonioConnector;
// STABLE
pub use sev_desk::SevDeskConnector;
// STABLE
pub use billomat::BillomatConnector;

// France connectors (WS10) — global market expansion.
// STABLE
pub mod qonto;
// STABLE
pub mod pennylane;
// STABLE
pub mod payfit;
// STABLE
pub mod colissimo;
// STABLE
pub mod cdiscount;
// STABLE
pub mod mangopay;
// STABLE
pub mod sendinblue;
// STABLE
pub mod ovh_cloud;
// STABLE
pub mod alan;
// STABLE
pub mod swile;
// STABLE
pub use qonto::QontoConnector;
// STABLE
pub use pennylane::PennylaneConnector;
// STABLE
pub use payfit::PayFitConnector;
// STABLE
pub use colissimo::ColissimoConnector;
// STABLE
pub use cdiscount::CdiscountConnector;
// STABLE
pub use mangopay::MangoPayConnector;
// STABLE
pub use sendinblue::SendinblueConnector;
// STABLE
pub use ovh_cloud::OvhCloudConnector;
// STABLE
pub use alan::AlanConnector;
// STABLE
pub use swile::SwileConnector;

// Switzerland connectors (WS11) — global market expansion.
// STABLE
pub mod postfinance;
// STABLE
pub mod twint;
// STABLE
pub mod swiss_post;
// STABLE
pub mod bexio;
// STABLE
pub mod abacus;
// STABLE
pub mod ricardo;
// STABLE
pub mod digitec_galaxus;
// STABLE
pub mod six_payment;
// STABLE
pub mod klara;
// STABLE
pub mod beem;
// STABLE
pub use postfinance::PostFinanceConnector;
// STABLE
pub use twint::TwintConnector;
// STABLE
pub use swiss_post::SwissPostConnector;
// STABLE
pub use bexio::BexioConnector;
// STABLE
pub use abacus::AbacusConnector;
// STABLE
pub use ricardo::RicardoConnector;
// STABLE
pub use digitec_galaxus::DigitecGalaxusConnector;
// STABLE
pub use six_payment::SixPaymentConnector;
// STABLE
pub use klara::KlaraConnector;
// STABLE
pub use beem::BeemConnector;

// Australia connectors (WS12) — global market expansion.
// STABLE
pub mod myob;
// STABLE
pub mod afterpay;
// STABLE
pub mod australia_post;
// STABLE
pub mod employment_hero;
// STABLE
pub mod deputy;
// STABLE
pub mod tyro;
// STABLE
pub mod prospa;
// STABLE
pub mod seek;
// STABLE
pub mod campaign_monitor;
// STABLE
pub mod pinch;
// STABLE
pub use myob::MyobConnector;
// STABLE
pub use afterpay::AfterpayConnector;
// STABLE
pub use australia_post::AustraliaPostConnector;
// STABLE
pub use employment_hero::EmploymentHeroConnector;
// STABLE
pub use deputy::DeputyConnector;
// STABLE
pub use tyro::TyroConnector;
// STABLE
pub use prospa::ProspaConnector;
// STABLE
pub use seek::SeekConnector;
// STABLE
pub use campaign_monitor::CampaignMonitorConnector;
// STABLE
pub use pinch::PinchConnector;

// LatinAmerica connectors (WS13) — global market expansion.
// STABLE
pub mod mercado_libre;
// STABLE
pub mod rappi;
// STABLE
pub mod nubank_business;
// STABLE
pub mod pagseguro;
// STABLE
pub mod ifood;
// STABLE
pub mod vtex;
// STABLE
pub mod clip;
// STABLE
pub mod uala;
// STABLE
pub mod falabella;
// STABLE
pub mod correos_mexico;
// STABLE
pub use mercado_libre::MercadoLibreConnector;
// STABLE
pub use rappi::RappiConnector;
// STABLE
pub use nubank_business::NubankBusinessConnector;
// STABLE
pub use pagseguro::PagSeguroConnector;
// STABLE
pub use ifood::IFoodConnector;
// STABLE
pub use vtex::VtexConnector;
// STABLE
pub use clip::ClipConnector;
// STABLE
pub use uala::UalaConnector;
// STABLE
pub use falabella::FalabellaConnector;
// STABLE
pub use correos_mexico::CorreosMexicoConnector;

// SEAExpanded connectors (WS14) — global market expansion.
// UNSTABLE
pub mod shopee_regional;
// UNSTABLE
pub mod lazada_regional;
// UNSTABLE
pub mod sea_money;
// UNSTABLE
pub mod grab_pay;
// UNSTABLE
pub mod bukalapak;
// UNSTABLE
pub mod blibli;
// UNSTABLE
pub mod traveloka;
// UNSTABLE
pub mod airasia_superapp;
// UNSTABLE
pub mod myeg;
// UNSTABLE
pub mod gcash;
// UNSTABLE
pub use shopee_regional::ShopeeRegionalConnector;
// UNSTABLE
pub use lazada_regional::LazadaRegionalConnector;
// UNSTABLE
pub use sea_money::SeaMoneyConnector;
// UNSTABLE
pub use grab_pay::GrabPayConnector;
// UNSTABLE
pub use bukalapak::BukalapakConnector;
// UNSTABLE
pub use blibli::BlibliConnector;
// UNSTABLE
pub use traveloka::TravelokaConnector;
// UNSTABLE
pub use airasia_superapp::AirAsiaSuperAppConnector;
// UNSTABLE
pub use myeg::MyEGConnector;
// UNSTABLE
pub use gcash::GCashConnector;
