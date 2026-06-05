# Connector Protocol

This document specifies the connector boundary: how Knowledge ingests
from external source systems. It is the reference companion to
[design.md §10.2](design.md) and [architecture.md §4.1](architecture.md)
and is implemented by the `connector_framework` crate
(`crates/connector_framework`), with individual providers in
`connectors`.

## One connector per source system

Every external system the substrate ingests from sits behind one
`Connector` instance. The framework ships the boundary; each provider
(Google Drive, OneDrive, Notion, Jira, Confluence, Figma, HubSpot,
Slack, Email, GitHub) is its own implementation.

## Authentication (OAuth2)

Authentication is OAuth2. Tokens are stored in an `OAuth2TokenVault` and
refreshed through a pluggable `TokenRefresher` hook, so credential
lifecycle is uniform across providers and refresh logic is not
duplicated per connector.

## Sync state machine

Sync runs provider cursors through `SyncState`, which carries a
connector through the full lifecycle:

```
full  →  incremental  →  failure  →  recovery
```

- **full** — initial backfill of the source.
- **incremental** — delta sync from the last provider cursor.
- **failure / recovery** — transient errors transition to a failure
  state with a recovery path rather than losing the cursor.

### Real content fetching

Connectors perform real document-content fetching, not just metadata
sync. On each sync a connector:

1. Discovers new/changed documents via the provider's delta API.
2. Fetches the full document body (respecting provider rate limits).
3. Ingests the content into the substrate evidence store.
4. Emits `DocumentCreated` / `DocumentUpdated` events.

## Push subscriptions (webhooks)

Where a provider supports push, a `WebhookSubscription` describes the
subscription and `parse_webhook_event` normalizes inbound payloads. The
optional embedded webhook receiver is gated behind the
`webhook-server` feature.

## Scope attachment and permissions

Each connector is attached to exactly one substrate scope via
`ConnectorAttachment` / `AttachmentRegistry`. Permission to attach or
detach is gated through `permission_service` (see
[permission-model.md](permission-model.md)). Source-system ACLs are
projected into the substrate's relation graph by `AclSyncEngine`, so a
document's reachability in Knowledge mirrors its access control in the
source system.

## Connector maturity

All 40 built-in connectors are **stable**. New contributed connectors
land **unstable** and graduate once they have soaked against the live
API. See [../product/roadmap.md](../product/roadmap.md#connector-maturity)
for the full status table.

## Writing a connector

Implement the `Connector` trait and wire authentication, sync, and
(optionally) webhook handling. See the step-by-step
[add-a-connector guide](../guides/add-a-connector.md).

## Further reading

- [design.md §10.2](design.md) — connector design rationale.
- [architecture.md §4.1](architecture.md) — connector boundary in the component map.
- [permission-model.md](permission-model.md) — ACL projection.
- [../guides/add-a-connector.md](../guides/add-a-connector.md) — authoring guide.
