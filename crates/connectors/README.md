# connectors

Vendor-specific connector implementations for the Knowledge substrate.

## Purpose

Ships nine concrete `Connector` implementations against the most
common B2B source systems. Each connector models the vendor's REST
contract as plain serde types and issues real HTTP through an
injected `HttpTransport`.

## Connectors

| Connector | Source |
|---|---|
| `GoogleDriveConnector` | Google Drive API v3 |
| `OneDriveConnector` | Microsoft Graph |
| `NotionConnector` | Notion API |
| `JiraConnector` | Jira REST v3 |
| `ConfluenceConnector` | Confluence REST |
| `FigmaConnector` | Figma REST |
| `HubSpotConnector` | HubSpot CRM v3 |
| `SlackConnector` | Slack Web API + Events API |
| `EmailConnector` | Gmail API + Microsoft Graph |

## Feature flags

| Feature | Description |
|---|---|
| `http-client` | Forwards to `connector_framework/http-client`. |
| `test-support` | Forwards to `connector_framework/test-support`. |
| `live-integration` | Enables live provider integration tests. |

## Links

- [connector_framework](../connector_framework/) — Framework crate.
- [docs/technical/design.md](../../docs/technical/design.md) §10.2 — Connector contract.
- [docs/getting-started/for-developers.md](../../docs/getting-started/for-developers.md) — Consumer integration guide.
