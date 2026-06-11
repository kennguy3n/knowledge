# Running Knowledge for 5,000 SME tenants

This guide covers sizing and the multi-tenant fairness controls that keep
a single tenant from degrading the rest of the fleet at scale. It assumes
you have read [scaling.md](scaling.md) (what scales horizontally vs.
vertically) and [deployment-guide.md](deployment-guide.md) (topology).

The dominant 5k-tenant risk is the **shared, CPU-bound llama-server
synthesis path**: one tenant triggering many syntheses can occupy the
whole pool and starve the other 4,999. Three independent controls bound
that blast radius:

1. **Synthesis fair-share** — per-tenant concurrency cap + bounded queue
   over a global cap, in front of the synthesis trigger.
2. **Per-tenant quotas** — requests/min, syntheses/day, and an advisory
   storage soft cap, with safe defaults and per-tenant overrides.
3. **Per-tenant SLOs** — p50/p95/p99 latency + error-rate metrics with
   tenant-cardinality protection, plus error-budget recording rules.

These are layered: per-IP and per-tenant **rate limits** shed abusive
bursts at the edge, **quotas** bound sustained volume per tenant, and
**fair-share** schedules the scarce synthesis resource fairly among
whoever is within quota.

---

## 1. Synthesis fair-share

The gateway admits synthesis triggers through a per-tenant concurrency
cap backed by a bounded FIFO queue, sitting under a global cap that
matches the real llama-server pool. A tenant at its cap queues (bounded);
when the queue is full the request is shed with `429 Too Many Requests`
and a `Retry-After` header rather than piling up unboundedly.

| Knob (`values.yaml`) | Env var | Default | Meaning |
|---|---|---|---|
| `config.synthesis.tenantConcurrency` | `KNOWLEDGE_SYNTHESIS_TENANT_CONCURRENCY` | `2` | Max concurrent syntheses one tenant may hold. |
| `config.synthesis.tenantQueue` | `KNOWLEDGE_SYNTHESIS_TENANT_QUEUE` | `4` | Bounded per-tenant wait slots once at the cap. |
| `config.synthesis.globalConcurrency` | `KNOWLEDGE_SYNTHESIS_GLOBAL_CONCURRENCY` | `2` | Max concurrent syntheses across ALL tenants (matches the default 1-replica pool). |
| `config.synthesis.queueWait` | `KNOWLEDGE_SYNTHESIS_QUEUE_WAIT` | `5s` | How long a queued synthesis waits before shedding. |

**Sizing rule.** `globalConcurrency` must track the llama-server pool so
it is saturated but never oversubscribed:

```
globalConcurrency ≈ llamaServer.replicaCount × per-replica parallelism
```

The baked-in SLM serves roughly one synthesis per core at a time; with
the default `2`-core limit per replica, budget ≈ 2 concurrent per
replica. So a 4-replica pool ⇒ `globalConcurrency: 8`. Keep
`tenantConcurrency` small (1–2) so no single tenant can hold more than a
fraction of the pool: with `tenantConcurrency: 2` and
`globalConcurrency: 8`, one tenant can occupy at most 25% of capacity.

**Defaults vs. the 5k recommendation.** The chart ships an eval-friendly
default of a **1-replica** pool (`llamaServer.replicaCount: 1`) with
`globalConcurrency: 2` — i.e. the global cap already matches the default
pool so it is *not* oversubscribed out of the box (`tenantConcurrency: 2`,
`tenantQueue: 4`). For a real 5k-SME deployment, scale the pool up and
raise the global cap in step — a **4-replica** pool with
`globalConcurrency: 8` is the recommended baseline:

```yaml
llamaServer:
  replicaCount: 4
config:
  synthesis:
    tenantConcurrency: 2   # one tenant ≤ 25% of an 8-wide pool
    globalConcurrency: 8   # 4 replicas × ~2 concurrent
```

SME synthesis is bursty but low-rate, so a modestly-sized fair-shared
pool absorbs the aggregate while the per-tenant cap guarantees fairness.
The invariant to preserve whenever you tune this: keep
`globalConcurrency ≈ replicaCount × ~2` so the pool is saturated but never
oversubscribed, and keep `tenantConcurrency` a small fraction of
`globalConcurrency` so no single tenant can monopolise the pool.

---

## 2. Per-tenant quotas

Quotas bound sustained per-tenant volume. They are resolved from the
tenant store behind a short TTL cache (so the hot path never hits the
database) and enforced in the gateway keyed on the authenticated tenant.
Hard quotas return `429` + `Retry-After`; the storage cap is advisory.

| Dimension | Default | Enforcement |
|---|---|---|
| Requests / minute | `1200` (20 rps sustained) | Hard — `429` when exceeded. |
| Syntheses / day | `500` | Hard — `429` when exceeded. |
| Storage soft cap | `50 GiB` | **Advisory** — response header + metric, never blocks. |

Defaults are deliberately generous for normal SME usage but bound runaway
clients. They are **fail-closed**: an unknown tenant or a transient store
error resolves to the safe default quota rather than an unbounded one.

**Per-tenant overrides.** Raise or lower any dimension for a single
tenant via the config API:

```http
PUT /api/v1/tenants/{id}/config
Content-Type: application/json

{
  "connector_limit": 10,
  "synthesis_tier": "standard",
  "retention_days": 365,
  "quota": { "requests_per_min": 6000, "syntheses_per_day": 2000, "storage_soft_cap_bytes": 107374182400 }
}
```

Omitting the `quota` object leaves the tenant's existing override
unchanged. A config change is reflected within the cache TTL (30s); wiring
the cache's invalidation hook makes a lowered quota (e.g. throttling an
abusive tenant) take effect immediately.

---

## 3. Sizing the deployment

### Gateway (stateless — scale horizontally)

Enable the HPA in production. The gateway carries no server-side session
state (tenant context lives in the JWT), so it scales out freely.

```yaml
autoscaling:
  enabled: true
  minReplicas: 3        # spread across availability zones
  maxReplicas: 20
  targetCPUUtilizationPercentage: 70
gateway:
  resources:
    requests: { cpu: "250m", memory: "256Mi" }
    limits:   { cpu: "1",    memory: "512Mi" }
```

For 5k SMEs, baseline traffic is dominated by ingest/query rather than
synthesis. Start at `minReplicas: 3` and let the HPA find the ceiling;
`maxReplicas: 20` at the default request size is ample headroom. The
gateway is I/O-bound (Postgres, NATS, substrate), so CPU-target scaling
tracks load well.

### llama-server (CPU-bound synthesis pool — scale horizontally)

```yaml
llamaServer:
  replicaCount: 4
  resources:
    requests: { cpu: "1",  memory: "1Gi" }
    limits:   { cpu: "2",  memory: "4Gi" }
config:
  synthesis:
    globalConcurrency: 8   # = replicaCount × ~2 concurrent/replica
```

This is the capacity knob that matters most at 5k tenants. Size it to the
fleet's *aggregate* synthesis rate, not the worst-case per-tenant burst —
fair-share already bounds the latter. Watch the synthesis backlog and p99
(below) and add replicas (and matching `globalConcurrency`) when the
queue-wait shed rate climbs.

### Substrate / Postgres / NATS

Unchanged by tenant count in kind — see [scaling.md](scaling.md). The
substrate is **vertical-only** (it owns a local SQLCipher store); Postgres
benefits from read replicas as the tenant table and audit volume grow.

---

## 4. SLO dashboard

Per-tenant SLO metrics and the error-budget recording/alerting rules are
documented in [slo.md](slo.md) and shipped in
[slo-recording-rules.yaml](slo-recording-rules.yaml). In summary:

- **Latency** — `knowledge_tenant_request_duration_seconds{route,tenant_id}`
  drives p50/p95/p99 per `(route, tenant)`, where `route` is one of the
  bounded classes `ingest` / `query` / `synthesis`.
- **Error rate** — `knowledge_tenant_slo_requests_total{route,tenant_id,outcome}`;
  `outcome="error"` is reserved for `5xx` (client `4xx`/`429` are not
  charged against the availability budget).
- **Cardinality protection** — `tenant_id` is capped at the 2,000 most
  active tenants with an `overflow` bucket, so series count stays bounded
  even at 5k+ tenants.

Recommended dashboard (templated on `tenant_id` and `route`):

1. **Availability** — `1 - error_ratio` against the 99.9% objective.
2. **Latency** — p50/p95/p99 lines with the p99 objective (1s for
   ingest/query) overlaid.
3. **Error-budget burn** — burn rate with the fast-burn page threshold
   (14.4×) marked.
4. **Fair-share health** — synthesis `429`/Retry-After shed rate and
   queue depth (capacity signal for adding llama-server replicas).

Two alerts ship in the rules: a **fast-burn page** (burn > 14.4 over 5m)
and a **latency warning** (p99 > 1s for 10m).

---

## Quick reference: the knobs

| Concern | Where | Default |
|---|---|---|
| Per-IP / per-tenant rate limit | `config.rateIpRps` / `config.rateTenantRps` | 50 / 200 rps |
| Synthesis per-tenant cap | `config.synthesis.tenantConcurrency` | 2 |
| Synthesis queue depth | `config.synthesis.tenantQueue` | 4 |
| Synthesis global cap | `config.synthesis.globalConcurrency` | 2 (= 1-replica pool; raise with `replicaCount`) |
| Requests/min quota | tenant config `quota.requests_per_min` | 1200 |
| Syntheses/day quota | tenant config `quota.syntheses_per_day` | 500 |
| Storage soft cap | tenant config `quota.storage_soft_cap_bytes` | 50 GiB |
| Gateway autoscaling | `autoscaling.*` | min 2 / max 10 @ 80% |
| Synthesis pool size | `llamaServer.replicaCount` | 1 |

## Further reading

- [scaling.md](scaling.md) — horizontal/vertical scaling model.
- [slo.md](slo.md) — per-tenant SLO model and recording rules.
- [monitoring.md](monitoring.md) — capacity signals and alerts.
- [configuration.md](configuration.md) — full environment-variable reference.
