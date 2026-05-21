//! `connectors` — vendor-specific [`Connector`](connector_framework::Connector)
//! implementations for the Knowledge substrate.
//!
//! Per `docs/DESIGN.md` §10.2 and `ARCHITECTURE.md` §4.1, the substrate
//! ingests evidence from external systems through the
//! [`connector_framework`] trait. This crate ships nine concrete
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

pub mod confluence;
pub mod email;
pub mod figma;
pub mod google_drive;
pub mod hubspot;
pub mod jira;
pub mod notion;
pub mod onedrive;
pub mod slack;

pub use confluence::ConfluenceConnector;
pub use email::EmailConnector;
pub use figma::FigmaConnector;
pub use google_drive::GoogleDriveConnector;
pub use hubspot::HubSpotConnector;
pub use jira::JiraConnector;
pub use notion::NotionConnector;
pub use onedrive::OneDriveConnector;
pub use slack::SlackConnector;
