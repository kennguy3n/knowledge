# HA Failover: RPO / RTO

This page documents the recovery objectives for the substrate's
active-passive high-availability mode and how they are measured and
enforced. For *how to enable* HA see
[deployment-guide.md → High availability](deployment-guide.md#high-availability-active-passive-failover);
this page is about *what to expect when the primary dies*.

## Summary

| Objective | Target | Measured (hermetic test) | Notes |
|---|---|---|---|
| **RPO** (max data loss) | 0 for acked WAL frames | **0 frames** | Only the in-flight segment not yet shipped can be lost — bounded by one checkpoint. |
| **RTO** (time to restore writes) | ≤ 2 × lease TTL | **< 2 × TTL** (≈ TTL + one election tick) | Default lease TTL is 15 s in production; the integration test uses 1 s. |

These are validated by the hermetic integration test
`crates/substrate_server/tests/ha_failover.rs`
(`primary_crash_promotes_standby_with_bounded_data_loss`) and the
gateway-side re-route test
`server/internal/substrate/client_ha_test.go`.

## Failover model

The substrate keeps all state in a single SQLCipher database, which
cannot scale out for writes. HA is therefore **active-passive WAL
shipping**:

1. The **primary** runs in `journal_mode=WAL`, and ships each committed
   transaction's frames over NATS JetStream. JetStream **acks** each
   published segment — once acked the frame is durably retained in the
   stream and replayable by any consumer.
2. One or more **standbys** subscribe to the stream, replay every frame
   into a local shadow database, and serve **read-only** queries.
3. **Leadership** is a single NATS KV lease with a TTL and a monotonic
   **epoch** (fencing token). The primary renews it every `TTL/3`. A
   standby that finds the lease expired steals it — which advances the
   epoch — and promotes itself: it switches its store to WAL mode and
   starts shipping.

The gateway routes writes to the primary and, on a `503`
standby/unreachable response, fails over to the configured standby URL
(`KNOWLEDGE_SUBSTRATE_URL_STANDBY`), so once a standby promotes the next
write lands on the new primary.

## RPO — Recovery Point Objective (data loss)

**RPO = 0 for acknowledged WAL frames.**

Every frame the primary shipped *and* JetStream acked before the crash
is retained in the durable stream and replayed by the promoting standby.
The promoted node resumes from the same watermark, so no acked write is
lost. The integration test asserts this directly: after a hard primary
crash the bus watermark is unchanged and every acked frame is still
present.

The **only** loss window is the transaction(s) committed locally on the
old primary but not yet shipped/acked when it died — at most one
in-flight WAL segment, i.e. bounded by one checkpoint interval. This is
inherent to asynchronous WAL shipping; synchronous replication would
trade it for write latency on every commit. For the 5k-SME multi-tenant
target we keep commits fast and accept a bounded, sub-second loss window
on hard primary loss.

To tighten RPO further:

- Lower the checkpoint/ship cadence so less data is ever in flight.
- Use synchronous (quorum) acks if/when the transport supports it —
  trades write latency for RPO.

## RTO — Recovery Time Objective (time to restore writes)

**RTO ≤ 2 × lease TTL** (typically ≈ one TTL plus a single election
tick).

When the primary crashes it stops renewing its lease. The lease lapses
once its TTL elapses from the **last** successful renewal; the worst case
is a crash immediately after a renewal (≈ a full TTL), the best case is a
crash just before expiry (≈ `TTL/3`, the renew cadence). A surviving
standby detects the lapse on its next election tick (`TTL/3`), steals the
lease, and promotes. So:

```
RTO ≈ (time to lease expiry: up to TTL) + (≤ one election tick: TTL/3)
    ≤ 2 × TTL  (with margin)
```

With the production default **lease TTL = 15 s**, expect writes to
resume within ~15–20 s of a hard primary loss with no operator action.
Reads are never interrupted: standbys serve throughout.

### Tuning RTO

The lease TTL is currently a fixed **15 s**
(`ReplicationConfig::from_env`). Lowering it shrinks RTO — at the cost of
more aggressive lease renewals and lower tolerance for transient NATS
latency (a too-short TTL risks spurious failovers when the primary is
merely slow). 15 s balances fast recovery against false-positive
promotions; values below a few seconds are unsafe in deployments with
non-trivial NATS round-trip latency. The integration test overrides the
TTL field directly to 1 s purely to keep the test fast.

## Reproducing the measurements

```bash
# Substrate failover (lease promotion, fencing, RPO = 0):
cargo test -p substrate_server --test ha_failover -- --nocapture

# Gateway write failover / read offload to the standby:
cd server && go test -race ./internal/substrate -run HA
```

The substrate test prints the measured promotion time, e.g.:

```
HA failover: crashed node-a; standby promoted in 690 ms \
  (lease TTL 1000 ms); RPO = 0 frames (watermark 5 retained)
```

The test uses the in-process WAL bus / lease store, whose
compare-and-set + TTL + monotonic-epoch semantics are identical to the
NATS JetStream / KV transports (see the `replication::memory` module
docs), so the promotion and fencing behaviour it asserts is the same one
the NATS-backed deployment runs — just with a 1 s TTL for speed.

## Monitoring failover

- `/health` exposes a `replication` object (`role`, `lag_frames`,
  `last_applied_at`).
- `/internal/metrics` exposes `knowledge_replication_lag_frames`.
- The bundled Grafana dashboard has a **Substrate Replication Lag
  (frames)** panel; Prometheus fires `KnowledgeReplicationLagHigh` when a
  standby falls >1000 frames behind (a leading indicator that a failover
  would replay a large backlog). See [monitoring.md](monitoring.md).
