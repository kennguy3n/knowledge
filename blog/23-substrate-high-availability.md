# High Availability for the Substrate

> **TL;DR:** The substrate is a single SQLCipher database, which cannot
> scale out — but it can now be made **highly available** through WAL
> shipping. A primary streams its committed WAL frames over NATS
> JetStream to one or more read-only standbys; leadership is held via a
> NATS key-value lease, and a standby promotes itself when the primary's
> lease expires. The gateway routes writes to the primary and offloads
> reads to a standby, and a `knowledge_replication_lag_frames` metric
> plus a `KnowledgeReplicationLagHigh` alert make lag observable.

## The Business Problem

Knowledge stores everything in one SQLCipher (SQLite) database. That is a
deliberate, load-bearing choice: a single encrypted file is what makes
on-device deployment, cryptographic forgetting, and the $0 cost model
work. But SQLite does not scale out horizontally, and for an enterprise
running the hybrid/enterprise tiers, a single substrate node is a single
point of failure. If that process or its host dies, writes stop until
someone restores from backup.

The goal of WS2 is to remove that single point of failure **without**
giving up the single-file model — to add failover, not a distributed
database.

## WAL shipping in one paragraph

SQLite in WAL journal mode appends every committed transaction's pages to
a `-wal` sidecar file as **frames**. That append-only frame log is, in
effect, a replication stream that already exists. The replication engine
reads new frames off the primary's WAL, packages them into `WalSegment`s,
and ships them to standbys, which replay them into a local shadow WAL and
serve **read-only** queries from the resulting state. The module parses
and re-emits the on-disk
[SQLite WAL format](https://sqlite.org/walformat.html) exactly — 32-byte
header, 24-byte frame headers, big-endian integers, and the rolling
checksum chain — and only ever ships the committed, intact prefix, which
is precisely what SQLite itself would recover.

## Transport-agnostic by design

The primary, standby, and failover loops are generic over two traits: a
`WalBus` (moves segments) and a `LeaseStore` (holds the leadership lock).
The default build ships an in-process implementation used by the unit
tests and single-host dev setups. The production transport — **NATS
JetStream** for the WAL stream plus a NATS **key-value** bucket for the
leadership lease — lives behind the non-default `replication-nats` cargo
feature, so standalone and cross-compile builds never link the
async-nats / TLS stack and stay lean.

Build the substrate image with the feature on to enable HA:

```bash
REPLICATION_NATS=1 docker compose -f deploy/docker-compose.yml build knowledge-substrate
```

## Leader election and failover

Exactly one node may write at a time. Leadership is a lease in the NATS
KV bucket: the primary holds it and renews it; standbys watch it. Roles
are set with `KNOWLEDGE_SUBSTRATE_ROLE` — `primary`, `standby`,
`disabled`, or `auto` (let the nodes elect a leader via the lease, the
recommended setting). When the primary's lease expires — because it
crashed, partitioned, or stalled — a standby wins the lease and promotes
itself to primary, switching from replaying frames to accepting writes.

The gateway participates in the failover. Point it at a standby with
`KNOWLEDGE_SUBSTRATE_URL_STANDBY`; it then routes **writes to the
primary** (failing over to the standby on a `503` standby/unreachable
response) and **offloads reads to the standby**, which both improves read
throughput and makes promotion transparent to clients.

## Running it

**Compose (single host).** `deploy/docker-compose.yml` ships a
commented-out `knowledge-substrate-standby` service. Build with
`REPLICATION_NATS=1`, uncomment the standby service and its volume, set
the roles (or run both as `auto`), and give the gateway
`KNOWLEDGE_SUBSTRATE_URL_STANDBY=http://knowledge-substrate-standby:9090`.

**Kubernetes (Helm).** Set `substrate.ha.enabled=true` and the chart
renders the substrate as a **StatefulSet** (one PVC per pod) instead of
the single Deployment. One pod is primary; `substrate.ha.replicas - 1`
are warm standbys (2 replicas is the recommended default). The gateway
addresses pods by their stable StatefulSet DNS names and routes around a
demoted or promoted node.

```bash
helm install knowledge deploy/helm/knowledge \
  --namespace knowledge --create-namespace \
  --set secrets.masterKey="$(openssl rand -hex 32)" \
  --set substrate.ha.enabled=true \
  --set substrate.ha.replicas=2
```

## Watching the lag

Replication is only trustworthy if you can see how far behind a standby
is. The substrate's `/health` endpoint gains a `replication` object
(`role`, `lag_frames`, `last_applied_at`, …), and `/internal/metrics`
exposes the headline gauge **`knowledge_replication_lag_frames`** — the
number of WAL frames a standby is behind the primary (flat at 0 on a
standalone node or the primary itself). The bundled Grafana dashboard
adds a **Substrate Replication Lag (frames)** panel, and the Prometheus
rules add a **`KnowledgeReplicationLagHigh`** alert that fires when a
standby falls more than 1000 frames behind. A standby that is steadily
falling behind is the early-warning sign that it would make a poor
failover target — so it is worth alerting on before, not during, an
incident.

## What it means for you

HA is opt-in and costs nothing when you do not use it: the default build
does not link the replication transport, and the default role is
`disabled`. When an enterprise needs failover, turning it on is a feature
flag and a standby node — not a migration to a different database. The
substrate stays a single encrypted file with all the privacy properties
that implies; it just gains a warm copy ready to take over.

## Further reading

- [deployment-guide.md](../docs/operator/deployment-guide.md#high-availability-active-passive-failover)
  — the full HA setup for Compose and Helm.
- [Observability Without Ops](12-observability-without-ops.md) — the
  metrics and alerting stack the lag gauge plugs into.
- [Sync Without Servers](11-sync-without-servers.md) — the *other* kind
  of replication (CRDT multi-device sync), and how it differs from HA.
