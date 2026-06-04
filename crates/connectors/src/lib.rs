//! `connectors` — vendor-specific [`Connector`](connector_framework::Connector)
//! implementations for the Knowledge substrate.
//!
//! Per `docs/technical/design.md` §10.2 and `docs/technical/architecture.md` §4.1, the substrate
//! ingests evidence from external systems through the
//! [`connector_framework`] trait. This crate ships ten concrete
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

// STABLE
pub mod confluence;
// STABLE
pub mod email;
// STABLE
pub mod figma;
// UNSTABLE
pub mod github;
// STABLE
pub mod google_drive;
// STABLE
pub mod hubspot;
// STABLE
pub mod jira;
// STABLE
pub mod notion;
// STABLE
pub mod onedrive;
// STABLE
pub mod slack;
// STABLE
pub mod stripe;
// STABLE
pub mod airtable;
// STABLE
pub mod bitbucket;
// STABLE
pub mod docusign;
// STABLE
pub mod gitlab;
// STABLE
pub mod miro;
// STABLE
pub mod quickbooks;
// STABLE
pub mod shopify;
// STABLE
pub mod trello;
// STABLE
pub mod xero;

// STABLE
pub use confluence::ConfluenceConnector;
// STABLE
pub use email::EmailConnector;
// STABLE
pub use figma::FigmaConnector;
// UNSTABLE
pub use github::GitHubConnector;
// STABLE
pub use google_drive::GoogleDriveConnector;
// STABLE
pub use hubspot::HubSpotConnector;
// STABLE
pub use jira::JiraConnector;
// STABLE
pub use notion::NotionConnector;
// STABLE
pub use onedrive::OneDriveConnector;
// STABLE
pub use slack::SlackConnector;
// STABLE
pub use stripe::StripeConnector;
// STABLE
pub use airtable::AirtableConnector;
// STABLE
pub use bitbucket::BitbucketConnector;
// STABLE
pub use docusign::DocuSignConnector;
// STABLE
pub use gitlab::GitLabConnector;
// STABLE
pub use miro::MiroConnector;
// STABLE
pub use quickbooks::QuickBooksConnector;
// STABLE
pub use shopify::ShopifyConnector;
// STABLE
pub use trello::TrelloConnector;
// STABLE
pub use xero::XeroConnector;
