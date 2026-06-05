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

A connector is **stable** once its trait implementation and test
coverage match the existing connectors and it has run cleanly against
the live provider; the 70 built-in connectors meet that bar. A
connector still soaking against a live API may instead land as
**unstable** as an honest signal to operators until it graduates.
Either way, note the status in the [roadmap](../product/roadmap.md).

## Built-in connectors

Knowledge ships **70 stable built-in connectors** — see the
[connector maturity table](../product/roadmap.md#connector-maturity)
for the full list. A built-in connector is a first-party module in the
`connectors` crate that is also registered as a `ConnectorKind`. Adding
one means wiring it in five places, not two:

1. `crates/connector_framework/src/config.rs` — add the `ConnectorKind`
   variant (and its `as_str()` arm).
2. `crates/connectors/src/lib.rs` — add `pub mod` / `pub use` with a
   `// STABLE` or `// UNSTABLE` marker (use `// UNSTABLE` while a new
   connector is still soaking against the live provider).
3. `crates/ffi/src/types.rs` — add the matching `ConnectorKindTag`.
4. `crates/ffi/src/connector.rs` — wire the `build_connector` factory,
   `connector_source_tag`, and the two enum-translation matches.
5. `crates/ffi/src/webhook.rs` — classify the provider for webhook
   routing, if it subscribes to webhooks.

A custom connector for a source you don't ship upstream only needs the
trait impl (steps above under [Steps](#steps)) — not the `ConnectorKind`
wiring.

## Further reading

- [connector-protocol.md](../technical/connector-protocol.md) — the model.
- [permission-model.md](../technical/permission-model.md) — ACL projection.
- [CONTRIBUTING.md](../../CONTRIBUTING.md) — PR workflow.
