//! `connectors` — vendor-specific [`Connector`](connector_framework::Connector)
//! implementations for the Knowledge substrate.
//!
//! Per `docs/technical/design.md` §10.2 and `docs/technical/architecture.md` §4.1, the substrate
//! ingests evidence from external systems through the
//! [`connector_framework`] trait. This crate ships twenty concrete
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

// UNSTABLE
pub mod airtable;
// UNSTABLE
pub mod bitbucket;
// STABLE
pub mod confluence;
// UNSTABLE
pub mod docusign;
// STABLE
pub mod email;
// STABLE
pub mod figma;
// STABLE
pub mod github;
// UNSTABLE
pub mod gitlab;
// STABLE
pub mod google_drive;
// STABLE
pub mod hubspot;
// STABLE
pub mod jira;
// UNSTABLE
pub mod miro;
// STABLE
pub mod notion;
// STABLE
pub mod onedrive;
// UNSTABLE
pub mod quickbooks;
// UNSTABLE
pub mod shopify;
// STABLE
pub mod slack;
// UNSTABLE
pub mod stripe;
// UNSTABLE
pub mod trello;
// UNSTABLE
pub mod xero;

// UNSTABLE
pub use airtable::AirtableConnector;
// UNSTABLE
pub use bitbucket::BitbucketConnector;
// STABLE
pub use confluence::ConfluenceConnector;
// UNSTABLE
pub use docusign::DocuSignConnector;
// STABLE
pub use email::EmailConnector;
// STABLE
pub use figma::FigmaConnector;
// STABLE
pub use github::GitHubConnector;
// UNSTABLE
pub use gitlab::GitLabConnector;
// STABLE
pub use google_drive::GoogleDriveConnector;
// STABLE
pub use hubspot::HubSpotConnector;
// STABLE
pub use jira::JiraConnector;
// UNSTABLE
pub use miro::MiroConnector;
// STABLE
pub use notion::NotionConnector;
// STABLE
pub use onedrive::OneDriveConnector;
// UNSTABLE
pub use quickbooks::QuickBooksConnector;
// UNSTABLE
pub use shopify::ShopifyConnector;
// STABLE
pub use slack::SlackConnector;
// UNSTABLE
pub use stripe::StripeConnector;
// UNSTABLE
pub use trello::TrelloConnector;
// UNSTABLE
pub use xero::XeroConnector;
