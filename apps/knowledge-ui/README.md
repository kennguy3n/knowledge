# knowledge-ui

A reference **end-user** web UI for a Knowledge deployment: chat with a
scope, run hybrid search, browse synthesized memory and its decay state
machine, explore a derived concept graph, stream synthesis progress, and
cryptographically forget a conversation. It is a thin, fully client-side
client over the existing gateway REST surface — deploy it next to the
gateway and end users (not just operators) can use it immediately.

This is the consumer-facing counterpart to [`admin/`](../../admin), which
targets operators. It follows the same conventions (same visual palette,
typed fetch client, multi-stage Node→nginx image, same-origin reverse
proxy) but is built with **Next.js 14 (App Router)** and shipped as a
**static export**.

## Features

| Page | Route | Gateway calls |
| --- | --- | --- |
| Conversations | `/` | — (local registry) |
| Chat | `/chat/{scopeId}` | `POST /ingest`, `GET /memories`, `POST /synthesis/trigger`, `GET /synthesis/{id}/status?stream=true`, `POST /forget/{scope_id}` |
| Search | `/search` | `POST /query`, `GET /evidence/{id}` |
| Memory browser | `/memory` | `GET /memories` |
| Settings | `/settings` | `GET /health` |

- **Chat** — type a message to ingest it as evidence into the scope;
  the right-hand panel shows the scope's synthesized memory and lets you
  trigger synthesis and watch progress stream in over SSE.
- **Search** — hybrid full-text + semantic search. The gateway query
  endpoint is per-scope, so "All conversations" fans out across the
  scopes known to this browser and merges hits by score. Each hit lazily
  loads its full evidence record on expand.
- **Memory browser** — browse memory rows by scope and decay-state
  filter, see the decay state machine with live per-state counts, and
  explore the concept graph.
- **Concept graph** — an interactive, dependency-free SVG force graph.
  Because the gateway does not (yet) expose a typed concept-graph
  endpoint, the graph is **derived client-side** from the scope's memory
  rows: nodes are memories (sized by retention, coloured by state),
  edges are lexical-overlap relations between summaries, and an
  archived↔live overlap renders as a supersession edge. The renderer
  only depends on the `ConceptGraphData` shape (`src/lib/types.ts`), so
  swapping in a real graph endpoint later is a one-function change in
  `src/lib/concept-graph.ts`.
- **Cryptographic forget** — "Forget conversation" opens a confirmation
  dialog explaining irreversibility, then calls `POST /forget/{scope_id}`
  (DEK destruction).

## Architecture

```
Browser ──▶ nginx (this image, :3002)
              ├── /              → static Next.js export (out/)
              └── /api, /health, /metrics → reverse proxy ──▶ knowledge-gateway:8080
```

- `output: 'export'` (see `next.config.mjs`) emits a fully static site
  under `out/`; there is **no Node runtime in production** — nginx
  serves the files exactly like the admin SPA.
- All gateway calls happen **client-side, same-origin**: nginx
  reverse-proxies `/api`, `/health`, `/metrics` to the gateway, so no
  CORS configuration is needed in the default deployment.
- The bearer token (a gateway API key or a tenant JWT) is entered on the
  Settings page and stored only in `localStorage`; it is never baked
  into the image and is sent as `Authorization: Bearer <token>`.

### Dynamic chat route under static export

`/chat/{scopeId}` is a dynamic segment, but scope ids are arbitrary
UUIDs not known at build time. The export emits a single placeholder
page (`/chat/scope/`); nginx falls back to it for every `/chat/<id>`
deep link, and the client reads the real scope id from the URL at
runtime (`src/lib/useScopeId.ts`). In-app navigation never hits the
fallback — Next renders the route client-side with the correct param.

### Source of API types

`src/lib/types.ts` mirrors the gateway/substrate JSON contracts by hand
(there is no codegen yet). Keep it in sync with:

- `server/internal/gateway/{gateway,evidence,synthesis}.go`
- `crates/ffi/src/types.rs`

## Local development

```bash
npm install
npm run dev      # http://localhost:3002
```

`next dev` does **not** proxy the gateway. Point the UI at a running
gateway with `NEXT_PUBLIC_GATEWAY_BASE_URL` (the gateway must then allow
the dev origin via `KNOWLEDGE_CORS_ORIGINS`):

```bash
NEXT_PUBLIC_GATEWAY_BASE_URL=http://localhost:8080 npm run dev
```

In the built image this is left empty so calls go same-origin through
nginx.

## Checks

```bash
npm run lint        # next lint
npm run typecheck   # tsc --noEmit
npm run build       # next build → static export in out/
```

## Docker

```bash
docker build -t knowledge-ui apps/knowledge-ui
docker run -p 3002:3002 knowledge-ui
```

The nginx upstream is configurable so the same image works under compose
and Helm. `apps/knowledge-ui/nginx.conf` is an **envsubst template**
rendered on container start; only `GATEWAY_*` variables are substituted
(`NGINX_ENVSUBST_FILTER=GATEWAY_`) so nginx's own `$host`/`$uri` runtime
variables survive.

| Env var | Default | Purpose |
| --- | --- | --- |
| `GATEWAY_UPSTREAM` | `knowledge-gateway:8080` | `host:port` the API is proxied to |
| `GATEWAY_RESOLVER` | `127.0.0.11` | DNS resolver (Docker embedded DNS by default) |

## Deployment

### docker-compose

Already wired into [`deploy/docker-compose.yml`](../../deploy/docker-compose.yml)
as the `knowledge-ui` service on `${UI_PORT:-3002}`:

```bash
cd deploy && docker compose up -d knowledge-ui
# open http://localhost:3002
```

### Helm

Disabled by default; enable with `ui.enabled=true`:

```bash
helm upgrade --install knowledge deploy/helm/knowledge \
  --set ui.enabled=true
```

The chart points `GATEWAY_UPSTREAM` at the gateway Service and sets
`GATEWAY_RESOLVER` to the cluster DNS. The default
`ui.gatewayResolver` is the kubeadm CoreDNS ClusterIP (`10.96.0.10`);
override it for clusters that use a different one:

```bash
kubectl -n kube-system get svc kube-dns -o jsonpath='{.spec.clusterIP}'
```

## Customization

- **Branding / theme** — palette and layout live in
  `src/app/globals.css` (dark default + a light theme via the
  `data-theme` attribute). The brand mark/name is in
  `src/components/Sidebar.tsx`.
- **API base / auth** — `src/lib/api.ts`.
- **Concept graph derivation** — `src/lib/concept-graph.ts`.
- **Decay state machine copy** — `src/components/DecayStateMachine.tsx`.
