# Cost model

Per-user marginal cost of operating the substrate at scale, broken
down by deployment profile.

The estimates below cover **server-side** costs only; client
distribution (app stores, MDM, code signing) is excluded because it
is a fixed cost that does not scale with active-user count.

All dollar figures are listed at vendor list price, in USD, as of
2026-05. They are **upper bounds** — every figure below is the most
expensive option the substrate supports for the named workload.
Operators with committed-use discounts, reserved instances, or
custom inference contracts will pay less.

---

## Per-user breakdown

### B2C — 100 M users, on-device only

Every step of the substrate runs inside the host process on the
user's device. The server side stores **nothing** about the user.

| Component                          | Per-user marginal cost / month |
|------------------------------------|-------------------------------:|
| Evidence storage (SQLCipher)       | **$0** (on user device)        |
| At-rest encryption (XChaCha20)     | **$0** (on user device)        |
| Inference — channel recap          | **$0** (on-device SLM)         |
| Inference — domain / tenant recap  | **$0** (on-device SLM)         |
| Provenance signing (ML-DSA-65)     | **$0** (on user device)        |
| Permission service                 | **$0** (on user device)        |
| Sync (CRDT delta) — single device  | **$0** (no peers to sync to)   |
| Server-side per-user total         | **$0**                         |

Only marginal cost to the operator is **app distribution**:

- iOS App Store annual developer fee, amortised across active
  installs ($99 / installed-base) — call it $0 / user / month at
  100 M users.
- Android Play Store one-time $25 — $0 / user / month at any scale.
- Code-signing certificates (EV cert ≈ $300 / yr) — $0 / user /
  month at any scale.

**Net: $0 / user / month.** This is the headline differentiator
versus every server-side competitor.

### Hybrid — 10 M users, 1 K tenants

10 M individual accounts plus 1 K organisations using server-side
synthesis. The on-device evidence store still holds the canonical
copy; the server runs the managed synthesis endpoint and the
shared CRDT relay.

| Component                              | Per-user marginal cost / month |
|----------------------------------------|-------------------------------:|
| Evidence storage (on device)           | $0                             |
| At-rest encryption (on device)         | $0                             |
| Inference — channel recap (on device)  | $0                             |
| Inference — domain synthesis (managed) | ≤ $0.06¹                       |
| Inference — tenant synthesis (managed) | ≤ $0.03¹                       |
| Provenance signing (on device)         | $0                             |
| Permission service (on device)         | $0                             |
| CRDT delta relay (S3 + CloudFront)     | ≤ $0.01²                       |
| OAuth2 refresh tracking (Postgres RDS) | ≤ $0.005³                      |
| Audit log archive (S3 Glacier IA)      | ≤ $0.001⁴                      |
| **Per-user total**                     | **≤ $0.106 / user / month**    |

¹ At list-price $0.001 / synthesis on a 7B SLM in a Nitro Enclave,
~60 syntheses/user/month (≈ 2 / day domain, 1 / day tenant).
Rate-limited by [`HttpManagedEndpointSynthesizer`'s
`RateLimiter`](../../crates/synthesis_engine/src/rate_limiter.rs) so
a runaway client cannot exceed the per-tenant cap operators
configure; sequential dispatch via
[`SynthesisBatcher`](../../crates/synthesis_engine/src/batcher.rs)
keeps a many-scope flush inside the per-minute budget.

² CloudFront egress at $0.085 / GB × ~120 MB / user / month of
CRDT deltas, billed at the operator side because the substrate
publishes deltas through the operator's CDN.

³ OAuth2 refresh tracking is one row per integration; the row is
queried at most every refresh interval (Notion 60 min, Google 50
min, Microsoft 50 min). At RDS db.t4g.small list price, ≤ $0.005
per active connector instance per month.

⁴ One audit-log archive per tenant, written daily, retained for
365 days at Glacier IA list price. Per-user share at 1 K
tenants / 10 M users ≈ $0.001.

### Enterprise — 1 M users, 100 tenants with connectors

100 enterprise tenants pulling continuously from Notion, Google
Workspace, Microsoft 365, Slack, Salesforce, HubSpot, Confluence,
Jira, and ServiceNow. Server runs every connector instance on
behalf of the tenant.

| Component                            | Per-user marginal cost / month |
|--------------------------------------|-------------------------------:|
| Evidence storage (on device)         | $0                             |
| At-rest encryption (on device)       | $0                             |
| Inference — managed synthesis        | ≤ $0.15¹                       |
| Connector polling (compute + egress) | ≤ $0.40²                       |
| Connector OAuth2 refresh tracking    | ≤ $0.05                        |
| Connector webhook subscriptions      | ≤ $0.02                        |
| Audit log archive                    | ≤ $0.01                        |
| Permission cache (server-side)       | ≤ $0.03                        |
| **Per-user total**                   | **≤ $0.66 / user / month**     |

¹ Enterprise tenants run synthesis more aggressively (every active
scope, every hour during business hours) — ~150 syntheses /
user / month at $0.001 / call.

² Per-tenant aggregate API-call volume against the 9 connectors,
divided by 10 K avg seats per tenant. Capped by
[`ProviderRateLimiter`](../../crates/connector_framework/src/provider_rate_limiter.rs)
to stay inside each provider's per-app quota — at the default
50 req/s / provider host budget that costs ≤ $0.40 / user / month
of compute (one EC2 m7g.large per ~10 K active polling tasks at
list price).

---

## Measured performance backing these estimates

The following numbers are from the production Criterion benchmark
suite (`crates/benchmarks/`); see [benchmarks.md](../technical/benchmarks.md)
for the full methodology and reference hardware.

| Workload | Measured throughput | Cost implication |
|---|---|---|
| Evidence ingest | ~1,043 msgs/sec (100K corpus) | Single substrate instance handles 90M msgs/day |
| FTS phrase query | p50 13.56 ms (100K rows) | Sub-15ms retrieval without external search infra |
| Hybrid retrieval | 9.70 ms (10K rows, FTS + semantic + recency) | On-device hybrid is fast enough for real-time UX |
| Synthesis pipeline | 8.14 µs (machinery only, excl. LLM) | Pipeline overhead is negligible vs. SLM latency |
| AEAD encrypt 64 KB | 80.4 µs (778 MiB/s) | Encryption is not a cost bottleneck |
| Decay sweep | 5.26 ms / 100K objects (19M rows/sec) | Daily sweep covers even large tenants in <1s |
| Storage per message | 612 bytes (at 500K scale) | 500K messages ≈ 292 MB on-device |
| Connector sync | ~6,750 docs/sec (mock transport) | 10K-doc delta sync completes in <2s |

These numbers confirm the cost model's core claim: the on-device
substrate handles realistic workloads without server-side compute.

The ingest-throughput, FTS-latency and hybrid-retrieval rows above are
mirrored machine-readably in
[`perf-baselines.json`](perf-baselines.json) and enforced weekly by the
`competitor_benchmark` job in
[`.github/workflows/benchmarks.yml`](../../.github/workflows/benchmarks.yml),
which re-runs the Criterion suites and fails if any metric regresses
more than 10% from these baselines. Keep the two in sync when a baseline
is intentionally re-measured.

---

## Competitive comparison

The substrate ships every component on-device by default. Every
server-side competitor pays the corresponding cost on every
read, write, and sync:

> Competitor figures below are **publicly-reported, order-of-magnitude
> list prices**, not quotes for a specific contract; they illustrate the
> structural cost difference of on-device inversion, not a vendor
> benchmark.

| Workload                | Substrate (B2C) | Substrate (Enterprise) | Server-side competitor |
|-------------------------|----------------:|-----------------------:|-----------------------:|
| Evidence storage / GB   | $0              | ~$0                    | ~$0.023 (S3) + $0.10 (egress) per GB |
| At-rest encryption      | $0              | $0                     | $0.03 / 10 K KMS ops   |
| Inference / channel recap| $0             | $0                     | $0.005 / call (managed) |
| Inference / domain summary| $0            | ≤ $0.001               | $0.005 / call          |
| Sync / device           | $0              | ≤ $0.01                | ≤ $0.05 per MAU        |
| Audit log               | $0              | ≤ $0.01                | $0.50 / GB / month     |

The reason every line item is ≤ "competitor" by an order of
magnitude is **inversion**: the substrate makes the user's device
hold the canonical copy, do the encryption, and do the inference.
The server only ever sees a cap-protected synthesis call (with
the operator's own model behind it) or a CRDT delta blob it
cannot decrypt.

Beyond per-workload cost, the substrate also ships **140 on-device
connectors across 10 markets** (UK, Germany, France, Switzerland,
Australia, Latin America, and expanded SEA, plus the original Vietnam,
SEA and GCC coverage) — regional banking, accounting, payments,
shipping, HR and marketplace systems that the US-centric SaaS assistants
and the managed cloud ETL pipelines (Fivetran / Airbyte / Nango) do not
cover. Maturity is labelled honestly rather than inflated: **5 are
live-verified** against recorded real provider traffic via a
deterministic cassette-replay harness, and the **rest are
contract-stable** (full connector contract plus mock-fixture tests, not
yet exercised against a provider's live bytes). See
[add-a-connector.md](../guides/add-a-connector.md) for the maturity
ladder and [comparison.md](../product/comparison.md) for the full
feature-by-feature breakdown against Copilot, Glean, Notion AI,
Pinecone, Guru, Notion AI Q&A, Google NotebookLM and Mem.ai. Publishing this
liveness distinction at all is a trust differentiator versus vendors who
quote a large connector count without saying which are verified against
a live API.

---

## Key cost levers

### On-device SLM tier (free)

The default channel-recap and domain-summary tier runs entirely
on-device through `inference_router`'s llama.cpp / MLX
back-ends. Promoting a workload to the managed synthesis
endpoint is opt-in per [`SynthesisWindowManager`'s
`tier` parameter](../../crates/synthesis_pipeline/src/window.rs) —
operators can keep the entire workload on-device and pay $0 if
the on-device model meets quality bars for their user
population.

### Server-side synthesis (paid)

[`HttpManagedEndpointSynthesizer`](../../crates/synthesis_engine/src/managed_endpoint.rs)
is gated by:

1. [`RateLimiter`](../../crates/synthesis_engine/src/rate_limiter.rs)
   — operator-pinned per-minute cap, billed window-aligned.
2. [`SynthesisBatcher`](../../crates/synthesis_engine/src/batcher.rs)
   — serialises bursts through one shared limiter so a
   many-scope flush stays inside the cap.
3. [`EndpointConfig::max_tokens`](../../crates/synthesis_engine/src/managed_endpoint.rs)
   — hard cap on response tokens per call; the default 1 024
   tokens covers a domain-summary recap comfortably.

Together these mean the worst-case spend is
`max_per_minute × max_tokens × $price_per_token × 60 × 24 × 30`
per active endpoint per month — *bounded by config*, not by user
behaviour.

### Connector polling interval

Each connector instance polls on a tunable interval (default 5
min for Slack, 15 min for Notion, 60 min for Google Drive). Per
[`crates/connector_framework/src/sync.rs`](../../crates/connector_framework/src/sync.rs)
the substrate pulls only the delta since the last successful
sync; per
[`crates/connector_framework/src/provider_rate_limiter.rs`](../../crates/connector_framework/src/provider_rate_limiter.rs)
the aggregate outbound QPS against any one provider host is
capped by a token bucket so even an aggressive interval cannot
exceed the provider's per-tenant quota. Operators tune the
interval per tenant to trade freshness against per-tenant
compute cost.

#### Mobile defaults and the single-wake batch window

The background scheduler in
[`crates/ffi/src/sync_scheduler.rs`](../../crates/ffi/src/sync_scheduler.rs)
takes a [`PlatformHint`](../../crates/ffi/src/types.rs) so a
battery- and radio-constrained host can opt into a coarser,
wake-coalescing profile. A host selects it through the optional
fifth argument of the N-API `startSyncScheduler(handle, …,
"mobile")` (or [`start_sync_scheduler_for_platform`] on the
UniFFI surface):

| Lever | Desktop default | Mobile default |
| --- | --- | --- |
| `default_interval_secs` (`MOBILE_SYNC_INTERVAL_SECS`) | 15 min (`900`) | **30 min (`1800`)** |
| `tick_interval_secs` (`MOBILE_SYNC_TICK_SECS`) | 30 s | **60 s** |
| Per-instance `next_attempt_at` anchor | per-dispatch fresh `now` (staggered) | single batch timestamp (coalesced) |

Under `Mobile`, passing `0` for the interval / tick / backoff
arguments resolves to the table's mobile default instead of being
rejected, so a mobile host that does not want to hard-code cadence
numbers can call `startSyncScheduler(h, 0, 0, 0, "mobile")`.

The two cadence doublings each roughly halve a wake-frequency
line item: the 30-minute interval halves the number of *outbound
sync dispatches* per connector per day, and the 60-second tick
halves the scheduler's own *heartbeat* wakes (one runtime-mutex
acquisition + connector-map walk per tick).

The **batch window** is the third, qualitatively different lever.
The desktop scheduler deliberately *staggers* due instances —
each dispatch's `next_attempt_at` is computed from a fresh
post-dispatch timestamp, so a cohort that came due together drifts
apart and spreads load across future ticks (anti-thundering-herd
against shared upstreams). The mobile scheduler does the opposite:
it anchors every instance dispatched in a tick to a single batch
timestamp, so the whole cohort comes due again in the *same*
future wake window. On a phone the dominant cost is the number of
times the CPU and radio are woken at all, not the spread of work
across a busy server, so coalescing every connector into one wake
is the correct trade. This is safe because the 30-minute mobile
interval dwarfs any realistic dispatch latency, so the
"next attempt scheduled in the past" hazard that motivates the
desktop stagger cannot arise.

[`start_sync_scheduler_for_platform`]: ../../crates/ffi/src/sync_scheduler.rs

### CRDT delta size

[`SyncEngine`](../../crates/sync_engine/src/lib.rs)'s `AddWinsSet`
tombstones grow monotonically until compacted; the new
[`compact_threshold`](../../crates/sync_engine/src/lib.rs) hook
(default 10 000 ops) auto-compacts so the steady-state delta
payload stays bounded. The bound directly controls per-user CDN
egress: at 120 MB / user / month for an actively-synced
user, dropping the threshold to 5 000 ops halves the egress
line item without affecting correctness (compaction is purely
storage representation).

---

## How these numbers stay honest

Every cost lever above maps to a config field that is read at
runtime — operators can pin a tighter cap any time and the cost
ceiling drops accordingly. Where a field is left unset, the
substrate applies a conservative default (e.g.
`DEFAULT_MAX_RPM = 60` for synthesis endpoints) so that an
unconfigured deployment has bounded cost by default. See
[`configuration.md`](configuration.md) for the full list of defaults.

The [`crates/synthesis_engine/`](../../crates/synthesis_engine/) and
[`crates/connector_framework/`](../../crates/connector_framework/)
test suites both exercise the rate-limiter and batcher paths so
a future regression that "always dispatches one extra request"
would surface as a test failure before it reached the cost
column.
