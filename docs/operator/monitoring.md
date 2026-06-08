# Monitoring

How to observe a Knowledge deployment: dashboards, metrics endpoints,
the metric catalogue, health probes, and alerting rules. The substrate
emits counters with **no PII**, so you get crash/health telemetry
without touching user content.

## Grafana

Open `http://localhost:3000` (default credentials `admin`/`admin` —
change these). The provisioned **Knowledge Overview** dashboard
includes:

| Panel | What it shows |
|---|---|
| Ingest Rate | `rate(knowledge_ingest_total[5m])` |
| Query Rate + Latency | Query ops/s and p50/p95/p99 latency |
| Synthesis Trigger/Success | Trigger, success, and throttle rates |
| Connector Sync by Provider | Per-provider success/failure rates |
| Error Breakdown by Kind | Error rates by `kind` label |
| Subsystem Health | Up/down status for each subsystem |
| Memory / CPU | Go process RSS and CPU usage |
| Postgres Pool Stats | Request throughput by route |

## Prometheus

Prometheus is available at `http://localhost:9091`. It scrapes:

- **knowledge-gateway** (`:8080/metrics`) — Go process + gateway
  counters.
- **knowledge-substrate** (`:9090/internal/metrics`) — Rust FFI
  counters.

## Metric catalogue

Gateway (Go):

- `knowledge_gateway_requests_total{method,route,status}`
- `knowledge_gateway_request_duration_seconds{method,route}`
- `knowledge_tenant_requests_total{tenant_id}`
- `knowledge_connector_sync_success_total{provider}`
- `knowledge_connector_sync_failure_total{provider}`
- `knowledge_synthesis_trigger_total`
- `knowledge_synthesis_success_total`
- `knowledge_synthesis_throttle_total`
- `knowledge_ingest_total`
- `knowledge_query_total`
- `knowledge_errors_total{kind}`
- `knowledge_subsystem_status{name}`

Substrate (Rust) — auto-generated from `ffi::MetricsSnapshot`:
`knowledge_ingest_total`, `knowledge_query_total`,
`knowledge_errors_total{by_kind}`, and the rest of the FFI counter set.

## Health endpoint

The gateway exposes a per-subsystem health probe:

```bash
curl -s http://localhost:8080/health | jq .
```

Each subsystem reports an up/down status; a `0` health gauge is what
fires `KnowledgeSubsystemDown` (see below). The substrate reports
degradation levels so partial failures surface before a hard outage.

## Alerting rules

Alerts are defined in `deploy/prometheus/alerts.yml`:

| Alert | Condition | Severity |
|---|---|---|
| `KnowledgeHighErrorRate` | >5% error rate for 5 minutes | warning |
| `KnowledgeSubsystemDown` | Health gauge == 0 for 2 minutes | critical |
| `KnowledgeSynthesisBacklog` | Pending syntheses stuck 30 minutes | warning |
| `KnowledgeConnectorFailing` | 3+ consecutive provider failures | warning |
| `KnowledgeHighLatency` | p99 latency >500ms for 5 minutes | warning |
| `KnowledgeDiskPressure` | Volume usage >80% for 5 minutes | warning |

When an alert fires, follow the [runbook](runbook.md) for the matching
subsystem.

## Further reading

- [runbook.md](runbook.md) — incident response per alert/subsystem.
- [troubleshooting.md](troubleshooting.md) — diagnostics for common
  issues.
- [deployment-guide.md](deployment-guide.md) — where the monitoring
  stack fits in the topology.
