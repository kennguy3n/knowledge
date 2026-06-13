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
- Add a **cassette replay test** (see below) to graduate to
  `live-verified` — this is what proves the full lifecycle without
  needing live credentials in CI.
- Run the workspace checks (`cargo fmt`, `cargo clippy`, `cargo test`)
  per [CONTRIBUTING.md](../../CONTRIBUTING.md).

### Cassette replay tests

A **cassette** is a committed JSON recording of the exact HTTP
request/response pairs a connector exchanges with its provider over a
full lifecycle. Replaying it drives the real connector code
(`authenticate` → `initial_sync` → `incremental_sync` →
`fetch_content` → `subscribe_webhook` / `handle_webhook_event` → ACL
projection) against the recorded bytes, deterministically and offline,
so CI verifies liveness without any live secret.

The harness lives in
[`connector_framework::cassette`](../../crates/connector_framework/src/cassette.rs)
(gated behind the `test-support` feature) and exposes two transports
that both implement `HttpTransport`:

- `ReplayTransport` — loads a `Cassette` and serves the recorded
  responses back, matching each outgoing request by `(method, url)` in
  FIFO order. An un-recorded request is a **hard error**, not a silent
  miss, so a stale fixture fails loudly. `assert_all_played()` then
  asserts every recorded interaction was consumed exactly once.
- `RecordingTransport` — wraps a real transport (e.g.
  `BlockingHttpTransport`), forwards each call, and appends the
  observed interaction to a cassette, **redacting** sensitive headers
  (`Authorization`, `Cookie`, `X-Api-Key`, …) before they hit disk.

Workflow to add one:

1. **Record once** against a provider sandbox: wrap the production
   transport in a `RecordingTransport`, run the connector lifecycle,
   then `cassette.save(path)`. Tokens in bodies are synthetic /
   sandbox values; sensitive headers are auto-redacted.
2. **Scrub & commit** the JSON under
   `crates/connectors/tests/cassettes/<provider>/`. Bodies stay
   human-readable (UTF-8 text) so the fixture is reviewable in a diff.
3. **Write the replay test** in
   [`crates/connectors/tests/cassette_replay.rs`](../../crates/connectors/tests/cassette_replay.rs),
   following the existing per-provider modules (`stripe`, `notion`,
   `momo`, `slack`, `github`). Point the connector's
   `auth_config_json.api_base_url` at the recorded base host, drive the
   lifecycle, exercise the ACL projection with `assert_acl_projection_round_trip`,
   call `replay.assert_all_played()`, and assert
   `ConnectorKind::<X>.maturity() == ConnectorMaturity::LiveVerified`.

These tests run in ordinary `cargo test` (no credentials) and also in
the weekly [`connectors-live-weekly`](../../.github/workflows/connectors-live-weekly.yml)
workflow's deterministic `cassette-replay` job.

## Maturity expectations

Maturity is an **honest, machine-readable** signal of how a connector's
contract has been *verified* — surfaced via `ConnectorKind::maturity()`
([`ConnectorMaturity`](../../crates/connector_framework/src/config.rs))
and mirrored in the [roadmap](../product/roadmap.md#connector-maturity).
The ladder:

1. **`unstable`** — trait implementation incomplete or still soaking;
   an honest signal to operators not to depend on it yet.
2. **`contract-stable`** — implements the full `Connector` contract
   with unit coverage at the `HttpTransport` boundary, but no committed
   cassette replays the whole lifecycle yet. This is the honest default
   for the bulk of the catalog.
3. **`live-verified`** — a committed cassette replays the full
   lifecycle deterministically in CI (see
   [Cassette replay tests](#cassette-replay-tests)). The current
   exemplars are GitHub, Slack, Notion, MoMo, and Stripe.

Graduate a connector by adding its cassette and flipping the
`ConnectorKind::maturity()` arm — never inflate the label without the
backing test.

## Built-in connectors

Knowledge ships **140 built-in connectors** (5 `live-verified`, the
rest `contract-stable`) — see the
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
