//! `connectors` — vendor-specific [`Connector`](connector_framework::Connector)
//! implementations for the Knowledge substrate.
//!
//! Per `docs/technical/design.md` §10.2 and `docs/technical/architecture.md` §4.1, the substrate
//! ingests evidence from external systems through the
//! [`connector_framework`] trait. This crate ships forty concrete
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
