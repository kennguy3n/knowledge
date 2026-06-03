# Quickstart

Three copy-pasteable deployment modes: on-device only, hybrid SME,
and enterprise full stack.

---

## Prerequisites (all modes)

- **Rust 1.85+**: `rustup install stable && rustup component add clippy rustfmt`
- **C toolchain** (bundled SQLCipher + OpenSSL): `sudo apt install build-essential` (Debian/Ubuntu)
- **Go 1.23+** (modes 2 and 3 only)
- **Docker** (mode 2 and 3 only, for Postgres/NATS)

---

## Mode 1: On-device only

Everything runs in-process. No server, no network, no cost.

```bash
# Clone and build
git clone https://github.com/kennguy3n/knowledge.git
cd knowledge
cargo build --all-targets --all-features --release

# Run the end-to-end demo
cargo run -p demo --release
```

The demo exercises the full substrate pipeline:

1. Opens an encrypted evidence store (SQLCipher)
2. Ingests multi-scope messages across multiple languages
3. Runs the observation engine (entity/fact/decision extraction)
4. Exercises the decay state machine and memory management
5. Builds the concept graph with supersession/contradiction edges
6. Triggers synthesis (using the NoOp fallback — no SLM required)
7. Exports portable concept profiles
8. Demonstrates cryptographic forgetting (DEK destruction)

Output: `results/demo_results.md`

### Wiring a real SLM (optional)

To run actual inference instead of the fallback synthesizer:

```bash
# Set env vars pointing to your llama-server binary and model
export LLAMA_SERVER_BINARY=/path/to/llama-server
export LLAMA_SERVER_MODEL=/path/to/bonsai-1.7b-q4_k_m.gguf

# The integration test drives a real synthesis through the router
cargo test -p integration_tests -- --ignored llama_server
```

---

## Mode 2: Hybrid SME (Go gateway + connectors)

A small team deployment: the Go gateway handles connector-based
ingestion from SaaS tools, while the Rust substrate runs alongside
as an HTTP loopback.

### Step 1: Start infrastructure

```bash
# Postgres + NATS (optional but recommended for production)
docker run -d --name knowledge-pg \
  -e POSTGRES_PASSWORD=knowledge \
  -e POSTGRES_DB=knowledge \
  -p 5432:5432 postgres:16-alpine

docker run -d --name knowledge-nats \
  -p 4222:4222 nats:latest -js
```

### Step 2: Start the substrate server

```bash
cd knowledge
cargo run -p substrate_server --release
# Listens on http://127.0.0.1:9090
```

### Step 3: Start the Go gateway

```bash
cd knowledge/server

# Required
export KNOWLEDGE_SUBSTRATE_URL=http://127.0.0.1:9090
export KNOWLEDGE_API_KEY=my-dev-key

# Optional (enables Postgres persistence + NATS audit)
export KNOWLEDGE_DATABASE_URL="postgres://postgres:knowledge@localhost:5432/knowledge?sslmode=disable"
export KNOWLEDGE_NATS_URL="nats://localhost:4222"

go run ./cmd/gateway
# Listens on http://127.0.0.1:8080
```

### Step 4: Connect a source (e.g., Notion)

```bash
API=http://localhost:8080/api/v1
KEY="Authorization: Bearer my-dev-key"

# Register a Notion connector
curl -s -X POST $API/connectors \
  -H "$KEY" -H "Content-Type: application/json" \
  -d '{
    "kind": "Notion",
    "scope_id": "22222222-2222-2222-2222-222222222222",
    "config_json": "{\"workspace_id\": \"your-notion-workspace-id\"}"
  }'

# Start OAuth flow (returns redirect URL)
curl -s "$API/connectors/<id>/oauth/start" -H "$KEY"

# After OAuth callback completes, trigger a sync
curl -s -X POST "$API/connectors/<id>/sync" -H "$KEY"
```

### Step 5: Query and synthesize

```bash
# Query ingested content
curl -s -X POST $API/query \
  -H "$KEY" -H "Content-Type: application/json" \
  -d '{"scope_id":"22222222-2222-2222-2222-222222222222","query_text":"project decision","limit":5}'

# Trigger synthesis
curl -s -X POST $API/synthesis/trigger \
  -H "$KEY" -H "Content-Type: application/json" \
  -d '{"scope_id":"22222222-2222-2222-2222-222222222222"}'

# Stream synthesis status (SSE)
curl -N "$API/synthesis/<syn-id>/status?stream=true" \
  -H "$KEY" -H "Accept: text/event-stream"
```

---

## Mode 3: Enterprise full stack (multi-tenant, all connectors)

All connectors active, multi-tenant isolation, TEE synthesis, SCIM
provisioning, audit log with retention.

### Step 1: Infrastructure

```bash
# Full infrastructure stack
docker run -d --name knowledge-pg \
  -e POSTGRES_PASSWORD=knowledge \
  -e POSTGRES_DB=knowledge \
  -p 5432:5432 postgres:16-alpine

docker run -d --name knowledge-nats \
  -p 4222:4222 nats:latest -js
```

### Step 2: Start services

```bash
# Terminal 1: substrate server
cargo run -p substrate_server --release

# Terminal 2: gateway with full config
cd server
export KNOWLEDGE_SUBSTRATE_URL=http://127.0.0.1:9090
export KNOWLEDGE_API_KEY=production-key
export KNOWLEDGE_JWT_SECRET=jwt-hmac-secret
export KNOWLEDGE_DATABASE_URL="postgres://postgres:knowledge@localhost:5432/knowledge?sslmode=disable"
export KNOWLEDGE_NATS_URL="nats://localhost:4222"
export KNOWLEDGE_RATE_IP_RPS=100
export KNOWLEDGE_RATE_TENANT_RPS=500
export KNOWLEDGE_RATE_BURST=200
export KNOWLEDGE_CORS_ORIGINS="https://app.example.com"
export KNOWLEDGE_PUBLIC_BASE_URL="https://api.example.com"
export KNOWLEDGE_SYNC_INTERVAL=5m

go run ./cmd/gateway
```

### Step 3: Create tenants and provision users

```bash
API=http://localhost:8080/api/v1
KEY="Authorization: Bearer production-key"

# Create a tenant
curl -s -X POST $API/tenants \
  -H "$KEY" -H "Content-Type: application/json" \
  -d '{"name": "Acme Corp"}'

# Provision users via SCIM v2
curl -s -X POST $API/scim/v2/Users \
  -H "$KEY" -H "Content-Type: application/json" \
  -d '{
    "schemas": ["urn:ietf:params:scim:schemas:core:2.0:User"],
    "userName": "alice@acme.com",
    "name": {"givenName": "Alice", "familyName": "Smith"}
  }'

# Grant permissions (Zanzibar tuples)
curl -s -X POST $API/permission/grant \
  -H "$KEY" -H "Content-Type: application/json" \
  -d '{
    "subject": {"subject_type": "user", "subject_id": "33333333-3333-3333-3333-333333333334"},
    "relation": "editor",
    "object": {"object_type": "scope", "object_id": "33333333-3333-3333-3333-333333333333"}
  }'
```

### Step 4: Connect all sources

Supported connectors (all with real document-content fetching):

| Provider | Auth | Gateway `/oauth/start` |
|---|---|:---:|
| Google Drive | `service_account_json` or OAuth2 | Yes |
| OneDrive / SharePoint | OAuth2 (Microsoft Graph) | Yes |
| Notion | `workspace_id`, OAuth2 | Yes |
| Jira | `site_url`, OAuth2 | Yes |
| Confluence | `site_url`, OAuth2 | Yes |
| Figma | `team_id`, OAuth2 | — ¹ |
| HubSpot | OAuth2 | — ¹ |
| Slack | `team_id`, OAuth2 | Yes |
| Email (IMAP) | `host`, `port`, credentials | N/A |
| GitHub _(unstable)_ | OAuth2 | Yes |

¹ Figma and HubSpot use OAuth2 via the Rust connector framework but
are not yet registered in the Go gateway's built-in OAuth flow starter.
Use `POST /api/v1/connectors/{id}/authenticate` with a pre-obtained
authorization code instead.

Each connector performs:
- OAuth2 token management and refresh
- Incremental delta sync (only new/changed documents)
- Webhook subscriptions for real-time updates
- Full document-content fetching (bodies, not just metadata)

### Step 5: Export and audit

```bash
# Export a portable concept profile
curl -s -X POST $API/export/profile \
  -H "$KEY" -H "Content-Type: application/json" \
  -d '{
    "scope_id": "33333333-3333-3333-3333-333333333333",
    "tenant_id": "tenant-acme",
    "actor": "user:alice",
    "format": "json"
  }'

# Query audit log
curl -s "$API/audit?tenant_id=tenant-acme&limit=50" -H "$KEY"
```

---

## Verification

After any mode, confirm the system is healthy:

```bash
# Health check
curl http://localhost:8080/health

# Prometheus metrics
curl http://localhost:8080/metrics

# Run the test suite
cargo test --all --all-features
cd server && go test -race -count=1 ./...
```

---

## Next steps

- [API_REFERENCE.md](API_REFERENCE.md) — full endpoint documentation
- [INTEGRATION_GUIDE.md](INTEGRATION_GUIDE.md) — embedding the substrate in your product
- [COST_MODEL.md](COST_MODEL.md) — production cost estimates
- [../ARCHITECTURE.md](../ARCHITECTURE.md) — system architecture and data flow
