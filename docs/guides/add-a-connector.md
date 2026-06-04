# Add a Connector

How to write a new connector so Knowledge can ingest from a source we
don't ship. Read [connector-protocol.md](../technical/connector-protocol.md)
first for the model; this guide is the authoring checklist.

## The contract

Every source sits behind one `Connector` implementation in the
`connectors` workspace, built on the `connector_framework` crate. You
implement four concerns:

1. **Authentication** — OAuth2 via the framework's `OAuth2TokenVault` +
   a `TokenRefresher` hook.
2. **Sync** — drive provider cursors through `SyncState`
   (`full → incremental → failure → recovery`).
3. **Content fetch** — discover changed documents, fetch full bodies
   (respecting rate limits), ingest, and emit `DocumentCreated` /
   `DocumentUpdated`.
4. **Push (optional)** — describe a `WebhookSubscription` and parse
   inbound events with `parse_webhook_event`.

## Steps

### 1. Create the crate / module

Add your connector alongside the existing providers in `connectors`.
Mirror an existing connector (e.g. Notion) as a template — match its
module layout and naming.

### 2. Implement `Connector`

Implement the trait from `connector_framework`. Wire:

- the OAuth2 client config (auth URL, token URL, scopes),
- the delta/discovery call against the provider API,
- the content-fetch call that returns full document bodies,
- the mapping from provider documents to substrate evidence.

### 3. Project source ACLs

If the source has its own access control, project it into the
permission graph via `AclSyncEngine` so reachability in Knowledge
mirrors the source. See
[permission-model.md](../technical/permission-model.md).

### 4. Wire feature flags

Network-backed behavior goes behind `http-client` (and
`async-http-client` / `webhook-server` if relevant), consistent with
the other connectors — see the feature-flag reference in the
[embed guides](embed-in-electron.md#4-feature-flags).

### 5. Handle rate limits and failures

Respect provider rate limits in the fetch loop, and make sure transient
failures transition `SyncState` to a recoverable failure rather than
losing the cursor.

### 6. Test

- Unit-test the document → evidence mapping.
- Add a live-integration test behind the `live-integration` feature
  (gated on env-var credentials) if you can.
- Run the workspace checks (`cargo fmt`, `cargo clippy`, `cargo test`)
  per [CONTRIBUTING.md](../../CONTRIBUTING.md).

## Maturity expectations

New connectors typically land as **unstable** until the provider
integration has soaked, then graduate to stable once the trait impl and
test coverage match the existing connectors. Note the status in the
[roadmap](../product/roadmap.md).

## Further reading

- [connector-protocol.md](../technical/connector-protocol.md) — the model.
- [permission-model.md](../technical/permission-model.md) — ACL projection.
- [CONTRIBUTING.md](../../CONTRIBUTING.md) — PR workflow.
