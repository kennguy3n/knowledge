# Deployment Scenarios

Knowledge ships in three deployment modes. This page maps business
shape to mode with a decision tree.

## The three modes

| Mode | Infrastructure | Marginal cost/user | Best for |
|---|---|---|---|
| **On-device** | None (embedded) | ~$0 | Native B2C apps; each user holds their own data. |
| **Hybrid (SME)** | Light Go gateway + connectors | Low | SMEs connecting SaaS tools, synthesis on-device or in a TEE. |
| **Enterprise** | Gateway + Postgres + NATS + MinIO + inference + monitoring | Higher (amortized) | Multi-tenant B2B with SCIM, audit, all connectors. |

## Decision tree

```
Is your app a native mobile/desktop client where each user
owns their own data?
│
├── Yes ─────────────────────────────────► Mode 1: On-device
│
└── No  → Do you need to pull from SaaS tools (Notion, Slack, Drive)
          and serve multiple users/teams?
          │
          ├── A few teams, minimal ops, want lowest infra ──► Mode 2: Hybrid (SME)
          │
          └── Many tenants, SCIM, audit, all connectors ───► Mode 3: Enterprise
```

## Mode 1 — On-device

The substrate is embedded directly in your app via `ffi`/`napi`. No
servers, no per-user inference bill, works offline. This is the KChat
pattern. Walkthrough: [Quickstart Mode 1](../QUICKSTART.md#mode-1-on-device-only).

## Mode 2 — Hybrid (SME)

A light Go gateway runs connectors and exposes a REST API; synthesis can
stay on-device or run in a trusted execution environment. Ideal for a
5–50 person company connecting a handful of SaaS tools with minimal ops.
Walkthrough:
[Quickstart Mode 2](../QUICKSTART.md#mode-2-hybrid-sme-go-gateway--connectors).

## Mode 3 — Enterprise

The full stack: multi-tenant isolation, SCIM provisioning, Zanzibar
permissions, audit log, TEE synthesis, and all connectors. Walkthrough:
[Quickstart Mode 3](../QUICKSTART.md#mode-3-enterprise-full-stack-multi-tenant-all-connectors).

## Migrating between modes

The substrate contract is the same across modes, so you can start
on-device and add a gateway later, or grow a hybrid SME deployment into
the enterprise topology, without re-architecting the data model.

## Further reading

- [for-operators.md](../getting-started/for-operators.md) — operator
  onboarding.
- [cost-model.md](../operator/cost-model.md) — cost per mode.
- [deployment-guide.md](../operator/deployment-guide.md) — production
  topology.
