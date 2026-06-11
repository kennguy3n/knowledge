# Per-tenant SLOs & error budgets

The gateway emits **per-tenant** latency and error-rate metrics for the
three SLO-relevant route classes — `ingest`, `query`, and `synthesis` —
so operators can hold a service-level objective per SME tenant and alert
on error-budget burn. This is the observability half of the 5k-tenant
fair-share/quota work: fair-share and quotas keep one tenant from
starving the rest, and these SLOs prove it (and catch regressions) per
tenant.

## Metrics

Emitted by `metrics.SLOMiddleware` (mounted after auth, so the tenant is
resolved in context):

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `knowledge_tenant_request_duration_seconds` | histogram | `route`, `tenant_id` | Request latency for SLO route classes (p50/p95/p99 source). |
| `knowledge_tenant_slo_requests_total` | counter | `route`, `tenant_id`, `outcome` | Request count by outcome (`success` / `error`). |

- **`route`** is a bounded class: `ingest`, `query`, or `synthesis`
  (never a raw path), so adding endpoints never grows cardinality.
- **`tenant_id`** is cardinality-capped at 2000 distinct values; beyond
  that, tenants are bucketed under `overflow` (shared with the existing
  `knowledge_tenant_requests_total` cap). This bounds series count to
  roughly `3 routes × 2001 tenants × (buckets|outcomes)` even if the
  tenant population spikes.
  - The cap is **claim-on-first-traffic and not auto-evicted**: the first
    2000 tenants to send traffic get individually-labelled SLO series;
    everyone else (and, at the documented 5k scale, the remaining ~3000)
    shares the `overflow` bucket. `overflow` is still budget-accurate in
    aggregate — it just isn't per-tenant attributable.
  - Decommissioned tenants do **not** free their slot automatically. This
    is deliberate: a time-based eviction sweep could thrash a quiet
    tenant in and out of `overflow`, making its SLO history vanish and
    reappear. To reclaim slots, restart the gateway (the map rebuilds
    from live traffic). If you need guaranteed per-tenant SLOs for a
    specific high-value tenant beyond the cap, raise `maxTenantLabels`
    in `server/internal/metrics/metrics.go` and accept the proportional
    Prometheus cardinality cost — don't reach for eviction.
- **`outcome`** is `error` for a `5xx` response and `success` otherwise.
  Client errors (`4xx`, including quota `429`s) are **not** charged
  against the availability error budget — they are client behaviour, not
  service failures.
- Long-lived SSE synthesis **status** streams (`/synthesis/{id}/status`)
  are excluded from the latency histogram (they would inflate p99) but
  are still counted for error-rate.

## Recording & alerting rules

Load [`slo-recording-rules.yaml`](./slo-recording-rules.yaml) into
Prometheus (`rule_files:`) or as a `PrometheusRule` CR. It defines:

- `tenant:knowledge_request_latency_seconds:p50|p95|p99` — per-tenant,
  per-route latency quantiles over a 5m window.
- `tenant:knowledge_requests:rate30m`,
  `tenant:knowledge_errors:rate30m`,
  `tenant:knowledge_error_ratio:ratio30m` — request/error rates and the
  error ratio in `[0,1]` (guarded against `0/0`).
- `tenant:knowledge_error_budget:burn30m` — burn rate against a **99.9%**
  availability objective (`error_ratio / 0.001`). `burn > 1` means the
  30-day budget is being consumed faster than sustainable.

Alerts:

- **`KnowledgeTenantErrorBudgetFastBurn`** (page) — 30m burn rate `>14.4`
  for 5m (the standard multi-window fast-burn threshold for a 99.9% SLO).
- **`KnowledgeTenantLatencyP99High`** (warning) — p99 `>1s` for
  `ingest`/`query` for 10m.
- **`KnowledgeTenantSynthesisLatencyP99High`** (warning) — p99 `>5s` for
  `synthesis` for 10m. Synthesis gets a separate, looser objective: the
  histogram already excludes the long-lived SSE status streams, so this
  watches the synchronous synthesis surface (trigger acceptance + recent
  listing). A sustained breach usually means the shared synthesis pool is
  saturated — raise `llamaServer.replicaCount` and
  `config.synthesis.globalConcurrency` together
  (see [multitenant-5k.md](./multitenant-5k.md)).

Tune the `0.001` budget, the `14.4` fast-burn factor, and the `1s`/`5s`
latency objectives to your contractual SLOs.

## Dashboard

A per-tenant SLO dashboard should template on `tenant_id` and `route`:

1. **Latency (p50/p95/p99)** — three series from the
   `tenant:knowledge_request_latency_seconds:*` recording rules, one row
   per route class.
2. **Error rate** — `tenant:knowledge_error_ratio:ratio30m` as a
   percentage, with the `0.1%` objective drawn as a threshold line.
3. **Error-budget burn** — `tenant:knowledge_error_budget:burn30m` with
   the `1` (sustainable) and `14.4` (fast-burn page) threshold lines.
4. **Top offenders** — `topk(10, tenant:knowledge_error_ratio:ratio30m)`
   to surface the noisiest tenants at a glance, including the `overflow`
   bucket.

Because the labels are bounded, these panels stay performant at 5k
tenants.
