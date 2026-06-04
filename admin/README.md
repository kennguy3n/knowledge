# Knowledge Admin

A lightweight, browser-based admin panel for managing a Knowledge
deployment without the CLI or PromQL. Built with **React + Vite +
TypeScript**.

It talks to the Go **gateway** (`server/`), which fronts the Rust
**substrate** (`crates/substrate_server`). All calls go through the
gateway's public HTTP surface (`/api/v1/...`, `/health`,
`/metrics/knowledge`).

## Pages

| Page          | What it does                                                        | Gateway endpoints |
| ------------- | ------------------------------------------------------------------- | ----------------- |
| Dashboard     | Aggregate health + headline metrics                                 | `GET /health`, `GET /metrics/knowledge` |
| Connectors    | List / create / sync / re-auth / delete connector instances         | `GET,POST /api/v1/connectors`, `POST /{id}/sync`, `GET /{id}/oauth/start`, `DELETE /{id}` |
| Tenants       | List / create / delete tenants, rotate keys, view members           | `GET,POST /api/v1/tenants`, `POST /{id}/key/rotate`, `GET /{id}/members`, `DELETE /{id}` |
| Synthesis     | Trigger a run, list recent runs, check run status                   | `POST /api/v1/synthesis/trigger`, `GET /synthesis/recent`, `GET /synthesis/{id}/status` |
| Memory        | Browse decaying memory objects for a scope by decay state           | `GET /api/v1/memories` |
| Audit log     | Query the tamper-evident audit event log                            | `GET /api/v1/audit` |
| Settings      | Gateway base URL, bearer token, cryptographic-forget danger zone    | `POST /api/v1/forget/{scope_id}` |

## Run locally

```bash
cd admin
npm install
npm run dev          # serves http://localhost:3001
```

`npm run dev` proxies `/api`, `/health`, and `/metrics` to the gateway.
By default it targets `http://localhost:8080`; override with:

```bash
KNOWLEDGE_GATEWAY_URL=http://localhost:8080 npm run dev
```

Bring up a gateway+substrate stack with `docker compose -f
deploy/docker-compose.yml up` (from the repo root) before using the
panel against live data. The panel also renders cleanly with no backend
(every page surfaces a typed error/empty state).

## Build

```bash
npm run build        # type-checks (tsc -b) then emits dist/
npm run preview      # serve the built bundle locally
npm run lint         # eslint
```

`VITE_GATEWAY_BASE_URL` (build-time) sets the API origin baked into the
bundle. Leave it empty (default) to call the gateway same-origin — which
is how the Docker image is wired (nginx reverse-proxies to the gateway).

## Docker

Multi-stage build (Node build → nginx serve), listening on **3001**:

```bash
docker build -t knowledge-admin admin/
```

In `deploy/docker-compose.yml` the `admin` service builds this image
and publishes `localhost:3001`, with nginx reverse-proxying `/api`,
`/health`, and `/metrics` to the `knowledge-gateway` service.

## Authentication

The gateway accepts a static API key **or** a tenant JWT as a
`Bearer` token (`server/internal/middleware`). Paste one into
**Settings → Authentication**; it is stored in `localStorage` and never
bundled into the image. When the gateway is started with no API key and
no JWT secret, it runs in dev-mode and accepts unauthenticated requests.

## Architecture notes / known gaps

- **Typed API client** lives in `src/api/`. `src/api/types.ts` mirrors
  the Go/Rust contracts by hand (no codegen). Where the gateway passes
  the substrate response through verbatim as opaque JSON
  (synthesis status/recent, memory rows), the types are intentionally
  permissive and the relevant fields are marked with
  `TODO(workstream-7)` until the substrate exposes documented schemas.
  This panel does **not** invent backend behavior or modify backend
  contracts.
- There is no list endpoint for a single connector's full config; the
  table renders the substrate `ConnectorStatus` projection.
