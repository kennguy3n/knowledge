# Scaling

How to scale a Knowledge deployment: horizontal scaling of the
stateless tier, vertical tuning of the stateful tier, and multi-region
considerations.

## What scales horizontally vs. vertically

| Component | Scaling axis | Why |
|---|---|---|
| Gateway (Go) | **Horizontal** | Stateless; instances share Postgres + NATS. No session affinity needed. |
| Substrate (Rust) | **Vertical** | Each instance owns a local SQLCipher store; scale CPU/RAM per node. |
| Postgres | **Vertical** (+ read replicas) | Relational store for tenants/permissions/audit. |
| NATS JetStream | **Horizontal** (cluster) | Event bus; cluster for throughput and HA. |
| MinIO | **Horizontal** (distributed mode) | Object store. |

## Horizontal scaling (gateway)

Run multiple gateway instances behind a load balancer. All instances
share the same Postgres and NATS cluster, and session affinity is not
required because tenant context is carried in the JWT, not server-side
session state. Tune per-IP and per-tenant rate limits
(`KNOWLEDGE_RATE_IP_RPS`, `KNOWLEDGE_RATE_TENANT_RPS`) to your traffic;
see [configuration.md](configuration.md).

## Substrate scaling

Each substrate instance maintains its own local SQLCipher store, so
scale it **vertically** (more CPU/RAM). For multi-node deployments,
each node runs its own substrate instance behind its own gateway; the
substrate loopback is never shared across nodes.

## Vertical tuning

- **Postgres** — raise `shared_buffers`, `work_mem`, and
  `max_connections` for higher concurrency.
- **NATS** — raise the JetStream storage limit (`--store_dir`,
  `--max_mem`).
- **Substrate** — adjust the SQLCipher cache for larger working sets.

## Multi-region

- Keep the gateway → substrate hop **in-region** (it's a loopback
  contract).
- Postgres: use a primary with cross-region read replicas; route writes
  to the primary.
- Respect data-residency: because user content lives in the substrate
  (on-device or in-region), there is no inherent cross-border transfer
  of user content. Keep it that way when placing regional substrates.

## Capacity signals

Watch these from [monitoring.md](monitoring.md):

- p99 query/ingest latency (the `KnowledgeHighLatency` alert).
- Synthesis backlog (`KnowledgeSynthesisBacklog`).
- Volume usage (`KnowledgeDiskPressure`).

## Further reading

- [configuration.md](configuration.md) — rate limits and tunables.
- [monitoring.md](monitoring.md) — capacity metrics.
- [deployment-guide.md](deployment-guide.md) — topology.
- [cost-model.md](cost-model.md) — cost at scale.
