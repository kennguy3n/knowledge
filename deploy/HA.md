# High Availability (active-passive substrate failover)

This is the deploy-time quickstart for the substrate's active-passive HA
mode. For the recovery objectives (RPO/RTO), the failover model, and how
they are measured, see
[docs/operator/ha-failover.md](../docs/operator/ha-failover.md).

## Recovery objectives at a glance

| Objective | Target | Measured |
|---|---|---|
| **RPO** (data loss) | 0 for acked WAL frames | 0 frames — only an unshipped in-flight segment can be lost |
| **RTO** (writes restored) | ≤ 2 × lease TTL | ≈ TTL + one election tick (≈ 15–20 s at the 15 s production TTL) |

Reads are never interrupted during a failover — standbys serve
read-only throughout. Only writes pause until a standby promotes.

## How it works

A **primary** ships its SQLite WAL frames over NATS JetStream; one or
more **standbys** replay them read-only. Leadership is a NATS KV lease
with a TTL and a monotonic epoch (fencing token). If the primary stops
renewing (crash), the lease lapses, a standby steals it (advancing the
epoch) and promotes itself to primary. The gateway routes writes to the
primary and fails over to `KNOWLEDGE_SUBSTRATE_URL_STANDBY` on a
standby/unreachable `503`.

The transport is gated behind the non-default `replication-nats` cargo
feature, so build the substrate image with it enabled:

```bash
REPLICATION_NATS=1 docker compose -f deploy/docker-compose.yml build knowledge-substrate
```

## Enable on Compose (single host)

1. Build with `REPLICATION_NATS=1` (above).
2. Set `KNOWLEDGE_SUBSTRATE_ROLE=auto` on both substrates (or `primary`
   on one and `standby` on the other for a pinned topology).
3. Uncomment the `knowledge-substrate-standby` service + its volume in
   `deploy/docker-compose.yml`.
4. Point the gateway at the standby:
   `KNOWLEDGE_SUBSTRATE_URL_STANDBY=http://knowledge-substrate-standby:9090`.

## Enable on Kubernetes (Helm)

```bash
helm upgrade --install knowledge deploy/helm/knowledge \
  --set substrate.ha.enabled=true \
  --set substrate.ha.replicas=2
```

This renders the substrate as a StatefulSet (one PVC per pod). Each pod
runs in `auto` role and competes for the lease; the gateway addresses
pods by their stable DNS names and routes around a demoted/promoted node.

> ⚠ Migrating an existing single-replica install to HA needs a volume
> migration — see
> [docs/operator/deployment-guide.md](../docs/operator/deployment-guide.md#high-availability-active-passive-failover).

## Verify

```bash
# Substrate self-promotion, fencing, and RPO = 0:
cargo test -p substrate_server --test ha_failover -- --nocapture

# Gateway write failover + read offload to the standby:
cd server && go test -race ./internal/substrate -run HA
```

## Monitor

- `/health` → `replication` object (`role`, `lag_frames`,
  `last_applied_at`).
- `/internal/metrics` → `knowledge_replication_lag_frames`.
- Grafana **Substrate Replication Lag (frames)** panel; Prometheus
  `KnowledgeReplicationLagHigh` alert (>1000 frames behind).
