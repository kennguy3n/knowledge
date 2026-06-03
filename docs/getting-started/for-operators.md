# Getting Started for Operators

You want to deploy and run Knowledge in production — connector-driven
ingestion, multi-tenant isolation, monitoring, and recovery.

## 1. Pick a deployment mode

| Mode | Infrastructure | Use it when |
|---|---|---|
| **On-device** | None (embedded in the app) | The app is native and each user holds their own data. |
| **Hybrid (SME)** | Go gateway + connectors | You connect SaaS tools but keep synthesis on-device or in a TEE. |
| **Enterprise** | Gateway + Postgres + NATS + MinIO + inference + Prometheus | Multi-tenant B2B with SCIM, audit, and all connectors. |

The [Quickstart](../QUICKSTART.md) has copy-pasteable setups for all
three. For choosing between them, see
[../product/deployment-scenarios.md](../product/deployment-scenarios.md).

## 2. Stand up the stack

Start with the [deployment guide](../operator/deployment-guide.md),
which covers the Docker Compose topology (gateway, substrate, Postgres,
NATS, MinIO, llama-server, Prometheus/Grafana), prerequisites, and the
production deployment checklist.

## 3. Configure

Everything is 12-factor (environment variables). The
[configuration reference](../operator/configuration.md) documents every
tunable, its default, and when to change it. Defaults are chosen so an
unconfigured deployment has bounded cost (e.g. conservative rate
limits).

## 4. Monitor

Wire up [monitoring](../operator/monitoring.md): Prometheus metrics,
the health endpoint, degradation levels, and alerting rules. The
substrate emits counters with no PII so you get crash/health telemetry
without touching user content.

## 5. Plan for scale and failure

- [scaling.md](../operator/scaling.md) — horizontal scaling, load
  balancing, multi-region.
- [backup-recovery.md](../operator/backup-recovery.md) — backup
  strategies, disaster recovery, key escrow.
- [troubleshooting.md](../operator/troubleshooting.md) — common issues
  and diagnostics.
- [runbook.md](../operator/runbook.md) — incident response per
  subsystem.

## 6. Understand cost and compliance

- [cost-model.md](../operator/cost-model.md) — per-user cost breakdown.
- [compliance.md](../operator/compliance.md) — SOC 2 / GDPR posture.

## What's next

Building the application on top? See
[for-developers.md](for-developers.md). Evaluating fit? See
[for-product-teams.md](for-product-teams.md).
