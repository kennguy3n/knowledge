# API Reference

REST API surface served by the Go gateway (`server/cmd/gateway`).
The gateway proxies evidence/synthesis operations to the Rust
substrate server via HTTP loopback and exposes connector, tenant,
permission, export, and audit services directly.

Base URL: `http://<host>:8080` (configurable via `KNOWLEDGE_GATEWAY_ADDR`).

---

## Authentication

All `/api/v1/*` routes require one of:

| Method | Header | Description |
|---|---|---|
| Static bearer token | `Authorization: Bearer <KNOWLEDGE_API_KEY>` | Service-to-service / admin calls |
| Tenant JWT | `Authorization: Bearer <jwt>` | Per-tenant access; HMAC-validated against `KNOWLEDGE_JWT_SECRET` |

When both are configured, the gateway checks the static key first;
if it does not match, it falls through to JWT validation.

> **⚠️ Dev-mode bypass:** When both `KNOWLEDGE_API_KEY` and
> `KNOWLEDGE_JWT_SECRET` are unset (empty), the gateway treats every
> request as an authenticated service principal — all endpoints are
> accessible without credentials. This is intended for local development
> only. **Never deploy to production with both secrets unset.**

Unauthenticated endpoints: `GET /health`, `GET /metrics`.

---

## Rate limiting

Two layers, applied in order:

1. **Per-IP** — token bucket keyed on the client IP (or
   `X-Forwarded-For` when `KNOWLEDGE_TRUSTED_PROXIES` is set).
   Default: 50 req/s, burst 100.
2. **Per-tenant** — token bucket keyed on the resolved tenant from
   the JWT. Default: 200 req/s, burst 100.

Rate-limited responses return `429 Too Many Requests` with:

```http
Retry-After: 1
```

Configuration:

| Env var | Default | Description |
|---|---|---|
| `KNOWLEDGE_RATE_IP_RPS` | 50 | Per-IP refill rate |
| `KNOWLEDGE_RATE_TENANT_RPS` | 200 | Per-tenant refill rate |
| `KNOWLEDGE_RATE_BURST` | 100 | Shared burst allowance (applies to both per-IP and per-tenant buckets) |
| `KNOWLEDGE_TRUSTED_PROXIES` | (empty) | Comma-separated CIDRs/IPs; when empty, XFF is ignored and rate limiter keys on transport peer |

---

## Error format

All errors return a flat JSON body (no wrapping `error` object):

```json
{
  "kind": "BadRequest",
  "message": "scope_id must be a UUID"
}
```

| Field | Type | Description |
|---|---|---|
| `kind` | string | Stable machine-readable error tag (`BadRequest`, `Unauthorized`, `Forbidden`, `NotFound`, `RateLimited`, `Internal`) |
| `message` | string | Human-readable detail (no PII) |

The HTTP status code is conveyed via the response status line, not in the
body (the struct carries it internally as `json:"-"`).

---

## Pagination conventions

List endpoints accept query parameters:

| Parameter | Default | Max | Description |
|---|---|---|---|
| `limit` | 20 | 1000 | Page size |
| `offset` | 0 | — | Number of items to skip |

Responses include metadata when paginated:

```json
{
  "items": [...],
  "total": 142,
  "limit": 20,
  "offset": 0
}
```

---

## Endpoints

### Evidence

#### `POST /api/v1/ingest`

Ingest a message into the evidence store.

**Request:**

```bash
curl -X POST http://localhost:8080/api/v1/ingest \
  -H "Authorization: Bearer $API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "scope_id": "11111111-1111-1111-1111-111111111111",
    "body": "We decided to use Rust for the rewrite.",
    "source": "Manual",
    "importance": "Important"
  }'
```

| Field | Type | Required | Default | Description |
|---|---|---|---|---|
| `scope_id` | UUID | yes | — | Privacy/crypto boundary |
| `body` | string | yes | — | Message content (non-empty UTF-8) |
| `source` | string | no | `"Manual"` | Source label |
| `importance` | string | no | `"Useful"` | `Critical` / `Important` / `Useful` / `Noise` |

**Response:** `201 Created`

```json
{
  "id": "ev-abc123"
}
```

---

#### `POST /api/v1/query`

Query the evidence store (hybrid FTS + semantic + recency retrieval).

**Request:**

```bash
curl -X POST http://localhost:8080/api/v1/query \
  -H "Authorization: Bearer $API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "scope_id": "11111111-1111-1111-1111-111111111111",
    "query_text": "Rust rewrite",
    "limit": 10
  }'
```

| Field | Type | Required | Default | Description |
|---|---|---|---|---|
| `scope_id` | UUID | yes | — | Scope to search within |
| `query_text` | string | yes | — | Search query (non-empty UTF-8) |
| `limit` | int | no | 20 | Max results to return |

**Response:** `200 OK`

```json
{
  "results": [
    {
      "id": "ev-abc123",
      "snippet": "We decided to use Rust for the rewrite.",
      "score": 0.95,
      "created_at": "2026-06-01T12:00:00Z"
    }
  ]
}
```

---

#### `GET /api/v1/evidence/{id}`

Retrieve a single evidence item by ID.

```bash
curl http://localhost:8080/api/v1/evidence/11111111-1111-1111-1111-111111111111 \
  -H "Authorization: Bearer $API_KEY"
```

**Response:** `200 OK` — full evidence record JSON.

---

#### `GET /api/v1/memories?scope_id={uuid}`

List memory objects for a scope.

```bash
curl "http://localhost:8080/api/v1/memories?scope_id=11111111-1111-1111-1111-111111111111" \
  -H "Authorization: Bearer $API_KEY"
```

**Response:** `200 OK` — array of memory objects.

---

#### `POST /api/v1/forget/{scope_id}`

Cryptographically forget a scope (DEK destruction).

```bash
curl -X POST http://localhost:8080/api/v1/forget/11111111-1111-1111-1111-111111111111 \
  -H "Authorization: Bearer $API_KEY"
```

**Response:** `204 No Content`

This is irreversible. The scope's DEK is destroyed, rendering all
ciphertext permanently unrecoverable.

---

### Reasoning

The reasoning plane answers questions that pure similarity retrieval
cannot: *what contradicts X*, *how has belief about X drifted*, and *why
was this answer retrieved*. All three are scope-bound: the gateway
validates `scope_id` as a UUID (fail-closed `400` otherwise) and the
substrate scans only that one scope's canonical claims, never another
scope's data. A scope with nothing to report — or a forgotten scope —
returns an empty result (`200`), never a `404`.

Contradiction and drift detection project the scope's user memory into a
single-scope concept graph capped at **256 highest-retention nodes**
(`REASONING_MAX_NODES` in `crates/ffi/src/reasoning.rs`). The cap bounds
the worst-case cost of the pairwise contradiction scan so one
pathologically large scope cannot starve the shared substrate; below the
cap every node is considered.

#### `POST /api/v1/reasoning/contradictions`

Return opposing canonical claims within a scope.

```bash
curl -X POST http://localhost:8080/api/v1/reasoning/contradictions \
  -H "Authorization: Bearer $API_KEY" \
  -H "Content-Type: application/json" \
  -d '{ "scope_id": "11111111-1111-1111-1111-111111111111" }'
```

| Field | Type | Required | Description |
|---|---|---|---|
| `scope_id` | UUID | yes | Scope to scan |

**Response:** `200 OK` — array of contradictions (stable order):

```json
[
  {
    "id": "c-abc123",
    "left_id": "22222222-2222-2222-2222-222222222222",
    "left_label": "Ship date is March 1",
    "right_id": "33333333-3333-3333-3333-333333333333",
    "right_label": "Ship date is April 15",
    "confidence": 0.82,
    "left_evidence_count": 3,
    "right_evidence_count": 2,
    "detected_at": "2026-06-01T12:00:00Z"
  }
]
```

#### `POST /api/v1/reasoning/drift`

Return canonical claims whose supporting evidence base has shifted.

```bash
curl -X POST http://localhost:8080/api/v1/reasoning/drift \
  -H "Authorization: Bearer $API_KEY" \
  -H "Content-Type: application/json" \
  -d '{ "scope_id": "11111111-1111-1111-1111-111111111111" }'
```

| Field | Type | Required | Description |
|---|---|---|---|
| `scope_id` | UUID | yes | Scope to scan |

**Response:** `200 OK` — array of drifting claims:

```json
[
  {
    "node_id": "44444444-4444-4444-4444-444444444444",
    "label": "Vendor is Acme Corp",
    "reason": "evidence_superseded",
    "evidence_at_promotion": 4,
    "evidence_remaining": 1,
    "detected_at": "2026-06-01T12:00:00Z"
  }
]
```

`reason` is one of `evidence_superseded`, `evidence_removed`, or
`evidence_weakened`.

#### `POST /api/v1/reasoning/explain`

Return the query planner's rationale for a retrieval — the *"why this
answer"* surface. The plan is a pure function of the query text, so this
route reads no scope data; the `scope_id` is still validated to keep the
authorization envelope uniform across the reasoning routes.

```bash
curl -X POST http://localhost:8080/api/v1/reasoning/explain \
  -H "Authorization: Bearer $API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "scope_id": "11111111-1111-1111-1111-111111111111",
    "query": "what was approved by finance"
  }'
```

| Field | Type | Required | Description |
|---|---|---|---|
| `scope_id` | UUID | yes | Validated for a uniform auth envelope |
| `query` | string | yes | Query text (non-empty UTF-8) |

**Response:** `200 OK` — the ordered retrieval plan:

```json
{
  "query": "what was approved by finance",
  "class": "relational",
  "steps": [
    { "mode": "graph_traversal", "cost_rank": 3, "time_budget_ms": null },
    { "mode": "fts", "cost_rank": 1, "time_budget_ms": null },
    { "mode": "raw_evidence", "cost_rank": 4, "time_budget_ms": null }
  ],
  "rationale": "Classified as relational: a relationship between entities, so typed-edge graph traversal is tried first. The substrate attempts the cheapest satisfying retrieval mode first and only falls back to a more expensive one if it misses: graph_traversal → fts → raw_evidence. The first mode that returns a hit answers the query; later modes are fallbacks.",
  "planned_at": "2026-06-01T12:00:00Z"
}
```

`class` is one of `point_lookup`, `relational`, `temporal`, `holistic`,
or `other`. `mode` is one of `summary`, `fts`, `semantic_vector`,
`graph_traversal`, or `raw_evidence`, with `cost_rank` 0–4 (lower is
cheaper). The chain is the planner's class-specific fallback order; the
first step that returns a hit answers the query and later steps are
fallbacks.

---

### Synthesis

#### `POST /api/v1/synthesis/trigger`

Trigger synthesis for a scope.

**Request:**

```bash
curl -X POST http://localhost:8080/api/v1/synthesis/trigger \
  -H "Authorization: Bearer $API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "scope_id": "11111111-1111-1111-1111-111111111111",
    "trigger": "ManualUserAction"
  }'
```

| Field | Type | Required | Default | Description |
|---|---|---|---|---|
| `scope_id` | UUID | yes | — | Scope to synthesize |
| `trigger` | string | no | `"ManualUserAction"` | Trigger reason |

**Response:** `202 Accepted`

```json
{
  "id": "syn-abc123"
}
```

---

#### `GET /api/v1/synthesis/{id}/status`

Get synthesis run status. Supports SSE streaming.

**JSON mode:**

```bash
curl http://localhost:8080/api/v1/synthesis/11111111-1111-1111-1111-111111111111/status \
  -H "Authorization: Bearer $API_KEY"
```

**Response:** `200 OK`

```json
{
  "status": "complete",
  "result": { ... }
}
```

**SSE streaming mode** (`Accept: text/event-stream` or `?stream=true`):

```bash
curl -N http://localhost:8080/api/v1/synthesis/11111111-1111-1111-1111-111111111111/status?stream=true \
  -H "Authorization: Bearer $API_KEY" \
  -H "Accept: text/event-stream"
```

The server polls the substrate every 1 second and emits named SSE
events. All frames include an `event:` type line — clients must use
`addEventListener('status', ...)` / `addEventListener('done', ...)` rather
than the default `onmessage` handler:

```
event: status
data: {"status":"running","progress":0.5}

event: status
data: {"status":"complete","result":{...}}

event: done
data: {"complete":true}

```

The stream closes when a terminal state is reached or after 300 polls
(5 minutes). On substrate errors:

```
event: error
data: {"message":"status unavailable"}

```

---

#### `GET /api/v1/synthesis/recent?scope_id={uuid}`

List recent synthesis runs for a scope.

```bash
curl "http://localhost:8080/api/v1/synthesis/recent?scope_id=11111111-1111-1111-1111-111111111111" \
  -H "Authorization: Bearer $API_KEY"
```

**Response:** `200 OK` — array of synthesis run summaries.

---

### Connectors

#### `POST /api/v1/connectors`

Register a new connector instance.

```bash
curl -X POST http://localhost:8080/api/v1/connectors \
  -H "Authorization: Bearer $API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "kind": "Notion",
    "scope_id": "11111111-1111-1111-1111-111111111111",
    "config_json": "{}"
  }'
```

---

#### `GET /api/v1/connectors`

List all registered connector instances.

---

#### `GET /api/v1/connectors/{id}/oauth/start`

Begin OAuth2 authorization flow. Returns a redirect URL.

---

#### `GET /api/v1/connectors/oauth/callback`

OAuth2 callback handler (called by the identity provider).

---

#### `POST /api/v1/connectors/{id}/authenticate`

Authenticate a connector with credentials.

---

#### `POST /api/v1/connectors/{id}/sync`

Trigger an incremental sync for a connector.

---

#### `GET /api/v1/connectors/{id}/status`

Get the current status of a connector.

---

#### `POST /api/v1/connectors/{id}/webhook/register`

Register a webhook subscription for real-time updates.

---

#### `POST /api/v1/connectors/{id}/webhook`

Receive incoming webhook payloads from providers.

---

#### `DELETE /api/v1/connectors/{id}`

Remove a connector registration.

---

### Tenants

#### `POST /api/v1/tenants`

Create a new tenant.

```bash
curl -X POST http://localhost:8080/api/v1/tenants \
  -H "Authorization: Bearer $API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"name": "Acme Corp"}'
```

---

#### `GET /api/v1/tenants`

List tenants.

---

#### `GET /api/v1/tenants/{id}`

Get tenant details.

---

#### `DELETE /api/v1/tenants/{id}`

Delete a tenant.

---

#### `PUT /api/v1/tenants/{id}/config`

Update tenant configuration.

---

#### `POST /api/v1/tenants/{id}/key/rotate`

Rotate the tenant's encryption key.

---

#### `GET /api/v1/tenants/{id}/members`

List tenant members.

---

#### `POST /api/v1/tenants/{id}/members`

Invite a member to the tenant.

---

#### `POST /api/v1/tenants/{id}/members/{userID}/activate`

Activate a member.

---

#### `POST /api/v1/tenants/{id}/members/{userID}/suspend`

Suspend a member.

---

#### `DELETE /api/v1/tenants/{id}/members/{userID}`

Remove a member from the tenant.

---

### Permissions

#### `POST /api/v1/permission/grant`

Grant a relation tuple.

```bash
curl -X POST http://localhost:8080/api/v1/permission/grant \
  -H "Authorization: Bearer $API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "subject": {"subject_type": "user", "subject_id": "22222222-2222-2222-2222-222222222222"},
    "relation": "viewer",
    "object": {"object_type": "scope", "object_id": "11111111-1111-1111-1111-111111111111"}
  }'
```

---

#### `POST /api/v1/permission/revoke`

Revoke a relation tuple.

---

#### `POST /api/v1/permission/check`

Check whether a subject has a relation to an object (Zanzibar
reachability check).

```bash
curl -X POST http://localhost:8080/api/v1/permission/check \
  -H "Authorization: Bearer $API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "subject": {"subject_type": "user", "subject_id": "22222222-2222-2222-2222-222222222222"},
    "relation": "viewer",
    "object": {"object_type": "scope", "object_id": "11111111-1111-1111-1111-111111111111"}
  }'
```

**Response:** `200 OK`

```json
{
  "allowed": true
}
```

---

### SCIM v2 (Identity Provisioning)

Standard SCIM v2 endpoints for user/group provisioning, mounted at
`/api/v1/scim/v2`:

| Method | Path | Description |
|---|---|---|
| `POST` | `/api/v1/scim/v2/Users` | Create user |
| `GET` | `/api/v1/scim/v2/Users` | List users |
| `GET` | `/api/v1/scim/v2/Users/{id}` | Get user |
| `PUT` | `/api/v1/scim/v2/Users/{id}` | Replace user |
| `DELETE` | `/api/v1/scim/v2/Users/{id}` | Delete user |
| `POST` | `/api/v1/scim/v2/Groups` | Create group |
| `GET` | `/api/v1/scim/v2/Groups` | List groups |
| `GET` | `/api/v1/scim/v2/Groups/{id}` | Get group |
| `PUT` | `/api/v1/scim/v2/Groups/{id}` | Replace group |
| `DELETE` | `/api/v1/scim/v2/Groups/{id}` | Delete group |

SCIM membership changes are joined to the Zanzibar tuple store
automatically, so group membership is reflected in permission checks.

---

### Export

#### `POST /api/v1/export/profile`

Render a portable concept profile for export.

```bash
curl -X POST http://localhost:8080/api/v1/export/profile \
  -H "Authorization: Bearer $API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "scope_id": "11111111-1111-1111-1111-111111111111",
    "tenant_id": "tenant-abc",
    "actor": "user:alice",
    "format": "json"
  }'
```

Export is policy-gated: the actor must have the appropriate relation
to the scope.

---

### Audit

#### `GET /api/v1/audit`

Query the audit log. Supports filtering by `tenant_id`, `action`,
`actor`, and time range via query parameters.

```bash
curl "http://localhost:8080/api/v1/audit?tenant_id=tenant-abc&action=ingest&limit=50" \
  -H "Authorization: Bearer $API_KEY"
```

---

### Operational

#### `GET /health`

Health check (unauthenticated). Returns subsystem status.

```bash
curl http://localhost:8080/health
```

**Response:** `200 OK` (healthy) or `503 Service Unavailable` (degraded)

```json
{
  "status": "ok",
  "subsystems": {
    "substrate": "ok",
    "postgres": "ok",
    "nats": "disabled"
  }
}
```

---

#### `GET /metrics`

Prometheus exposition (unauthenticated). Exports:

- `gateway_requests_total{method, route, status}` — request counter
- `gateway_request_duration_seconds{method, route}` — latency histogram

---

## Configuration reference

| Env var | Default | Description |
|---|---|---|
| `KNOWLEDGE_API_KEY` | (empty) | Static bearer token; empty disables bearer auth |
| `KNOWLEDGE_JWT_SECRET` | (empty) | HMAC secret for JWT validation; empty disables JWT |
| `KNOWLEDGE_GATEWAY_ADDR` | `:8080` | Gateway bind address |
| `KNOWLEDGE_SUBSTRATE_URL` | `http://127.0.0.1:9090` | Substrate server loopback URL |
| `KNOWLEDGE_DATABASE_URL` | (empty) | Postgres DSN; empty uses in-memory stores |
| `KNOWLEDGE_NATS_URL` | (empty) | NATS JetStream URL; empty disables audit consumer |
| `KNOWLEDGE_RATE_IP_RPS` | 50 | Per-IP refill rate (req/s) |
| `KNOWLEDGE_RATE_TENANT_RPS` | 200 | Per-tenant refill rate (req/s) |
| `KNOWLEDGE_RATE_BURST` | 100 | Shared burst allowance (both buckets) |
| `KNOWLEDGE_CORS_ORIGINS` | (empty) | Comma-separated CORS allow-list; empty allows `*` |
| `KNOWLEDGE_TRUSTED_PROXIES` | (empty) | CIDRs/IPs; empty ignores XFF (keys on transport peer) |
| `KNOWLEDGE_SYNC_INTERVAL` | 15m | Default connector sync cadence |
| `KNOWLEDGE_PUBLIC_BASE_URL` | `http://127.0.0.1:8080` | Public URL for OAuth redirect/webhook callbacks |
