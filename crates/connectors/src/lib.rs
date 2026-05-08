//! `connectors` — vendor-specific [`Connector`](connector_framework::Connector)
//! implementations for the Knowledge substrate.
//!
//! Per `PROPOSAL.md` §10.2 and `PHASES.md` Phase 4, the substrate
//! ingests evidence from external systems through the
//! [`connector_framework`] trait. This crate ships seven concrete
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
//!
//! Each connector models the vendor's REST contract as plain serde
//! types and parses fixture JSON into [`ConnectorEvent`]s. The
//! transport layer (HTTP client, retries, rate limiting) lives in
//! the Go gateway and is not part of this crate — connectors are
//! deliberately synchronous and side-effect-free against in-memory
//! fixtures so they can be exhaustively unit-tested.

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
