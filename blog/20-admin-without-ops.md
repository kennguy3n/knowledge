# Managing Knowledge Without a DevOps Team

> **TL;DR:** A new browser-based **admin dashboard** (React + Vite,
> served on `:3001`) lets a small team run a Knowledge deployment
> without the CLI or PromQL. Seven pages cover health, connectors,
> tenants, synthesis, the memory browser, the audit log, and settings —
> all talking to the gateway's public HTTP surface. Combined with
> pre-built images and a Helm chart, an SME can stand up and operate the
> stack without becoming accidental infrastructure operators.

## The Business Problem

The hybrid deployment makes Knowledge cheap to *run*, but someone still
has to operate it. For a 30-person company with no platform team, "edit
a YAML file, `curl` the gateway, read Prometheus metrics" is a non-
starter. They need to connect a few SaaS tools, see that sync is
healthy, trigger a synthesis run, and occasionally check the audit log —
and they need to do it from a browser, not a terminal.

That is exactly the gap the admin dashboard fills. It is intentionally
*not* a heavyweight control plane; it is a thin SPA over the gateway's
existing REST surface, so it adds an operator UI without adding a new
trust boundary or a database of its own.

## Seven pages, no terminal

The dashboard ships as a static React build served by nginx, which also
reverse-proxies API calls to the gateway. Every page maps directly onto
public gateway endpoints:

- **Dashboard** — aggregate health and headline metrics
  (`GET /health`, `GET /metrics/knowledge`).
- **Connectors** — list, create, sync, re-auth (OAuth start), and delete
  connector instances. This is where you wire up the
  [40-provider catalog](19-connector-ecosystem.md) without touching a
  config file.
- **Tenants** — create and delete tenants, rotate per-tenant keys, and
  view members.
- **Synthesis** — trigger a run and inspect recent runs and their
  status.
- **Memory** — browse decaying memory objects for a scope by decay
  state, so you can *see* the substrate remembering and forgetting.
- **Audit log** — query the tamper-evident audit event log.
- **Settings** — set the gateway base URL and bearer token, and run a
  cryptographic-forget from a clearly marked danger zone.

## How it fits the deployment

The dashboard is just another service in the compose file:

```bash
docker compose \
  -f deploy/docker-compose.yml \
  -f deploy/docker-compose.images.yml \
  up -d
# admin UI now on http://localhost:3001
```

Because it only needs network reachability to `knowledge-gateway` and
holds no state, it deploys the same way on Kubernetes via the Helm
chart, and it inherits whatever auth and TLS termination you put in
front of the gateway. There is no separate admin database to back up and
no extra secret to rotate.

## Why this matters

"No-ops" only works if *operating* the system is also no-ops. Pre-built
images removed the build step; the Helm chart removed the
hand-assembled manifests; the admin dashboard removes the CLI. Together
they let a team that connects its tools in the morning be answering
questions across them by the afternoon — without hiring a platform
engineer to keep the lights on.

## Further reading

- [deployment-guide.md](../docs/operator/deployment-guide.md#admin-dashboard)
  — the admin service, ports, and page-to-endpoint map.
- [70 Connectors](19-connector-ecosystem.md) — the catalog you wire up
  from the Connectors page.
- [Observability Without Ops](12-observability-without-ops.md) — what the
  dashboard's metrics are built on.
