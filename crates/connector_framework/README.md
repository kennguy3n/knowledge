# connector_framework

Connector boundary and framework for the Knowledge substrate.

## Purpose

Every external source system the substrate ingests from sits behind
one `Connector` instance. This crate provides the framework:
OAuth2 token management, sync state machine, webhook subscriptions,
scope attachment, ACL projection, and HTTP transport abstractions.
Individual connectors (Google Drive, OneDrive, etc.) live in the
sibling `connectors` crate.

## Public API summary

| Type / Function | Description |
|---|---|
| `Connector` | Trait that vendor connectors implement. |
| `ConnectorConfig` / `ConnectorKind` | Configuration and type enum. |
| `OAuth2TokenVault` / `TokenRefresher` | OAuth2 token lifecycle. |
| `SyncState` | Full → incremental → failure → recovery state machine. |
| `WebhookSubscription` / `parse_webhook_event` | Push subscription handling. |
| `AttachmentRegistry` | Connector-to-scope binding. |
| `AclSyncEngine` | Source-system ACL projection. |
| `BlockingHttpTransport` | Reqwest-backed HTTP (feature-gated: `http-client`). |
| `MockHttpTransport` | Test double (feature-gated: `test-support`). |

## Feature flags

| Feature | Description |
|---|---|
| `http-client` | Enables reqwest-backed `BlockingHttpTransport` + `OAuth2Client`. |
| `async-runtime` | Tokio + async-trait bridge for async connector driving. |
| `async-http-client` | Async reqwest transport. |
| `webhook-server` | Embedded HTTP webhook receiver. |
| `test-support` | Exposes `MockHttpTransport` outside `cfg(test)`. |

## Links

- [ARCHITECTURE.md](../../docs/technical/architecture.md) §4.1 — Connector service.
- [docs/DESIGN.md](../../docs/DESIGN.md) §10.2 — Connector contract.
- [docs/INTEGRATION_GUIDE.md](../../docs/INTEGRATION_GUIDE.md) — Consumer integration guide.
