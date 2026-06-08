# API Cookbook

Common patterns against the Go gateway REST API, with `curl`. For the
full endpoint reference (request/response schemas, error format,
pagination), see
[../technical/api-reference.md](../technical/api-reference.md).

Set up shell variables first:

```bash
API=http://localhost:8080/api/v1
KEY="Authorization: Bearer $KNOWLEDGE_API_KEY"
JSON="Content-Type: application/json"
```

## Ingest a message

```bash
curl -s -X POST $API/ingest -H "$KEY" -H "$JSON" -d '{
  "scope_id": "11111111-1111-1111-1111-111111111111",
  "body": "We decided to use Rust for the rewrite.",
  "importance": "Important"
}'
```

## Query

```bash
curl -s -X POST $API/query -H "$KEY" -H "$JSON" -d '{
  "scope_id": "11111111-1111-1111-1111-111111111111",
  "text": "Rust rewrite",
  "limit": 10
}'
```

## Read memories for a scope

```bash
curl -s "$API/memories?scope_id=11111111-1111-1111-1111-111111111111" -H "$KEY"
```

## Trigger synthesis and stream status (SSE)

```bash
SYN=$(curl -s -X POST $API/synthesis/trigger -H "$KEY" -H "$JSON" \
  -d '{"scope_id":"11111111-1111-1111-1111-111111111111"}' | jq -r .id)

curl -N "$API/synthesis/$SYN/status?stream=true" \
  -H "$KEY" -H "Accept: text/event-stream"
```

## Cryptographic forgetting

```bash
curl -s -X POST $API/forget/11111111-1111-1111-1111-111111111111 -H "$KEY"
```

After this returns, the scope's evidence bodies are unrecoverable — the
scope DEK is destroyed. See
[../technical/crypto-spec.md](../technical/crypto-spec.md).

## Connect a SaaS source (OAuth2)

```bash
# 1. Register the connector
CID=$(curl -s -X POST $API/connectors -H "$KEY" -H "$JSON" \
  -d '{"kind":"notion","scope_id":"11111111-1111-1111-1111-111111111111"}' | jq -r .id)

# 2. Start the OAuth2 flow (open the returned URL in a browser)
curl -s "$API/connectors/$CID/oauth/start" -H "$KEY"

# 3. After the callback completes, trigger a sync
curl -s -X POST $API/connectors/$CID/sync -H "$KEY"

# 4. Check status
curl -s "$API/connectors/$CID/status" -H "$KEY"
```

## Multi-tenant: create a tenant and grant access

```bash
# Create a tenant
TID=$(curl -s -X POST $API/tenants -H "$KEY" -H "$JSON" \
  -d '{"name":"Acme"}' | jq -r .id)

# Grant a user a relation on a scope
curl -s -X POST $API/permission/grant -H "$KEY" -H "$JSON" -d '{
  "object_type": "Channel", "object_id": "c-3",
  "relation": "viewer",
  "subject_type": "User", "subject_id": "u-7"
}'

# Check a permission
curl -s -X POST $API/permission/check -H "$KEY" -H "$JSON" -d '{
  "object_type": "Channel", "object_id": "c-3",
  "relation": "viewer",
  "subject_type": "User", "subject_id": "u-7"
}'
```

See [../technical/permission-model.md](../technical/permission-model.md)
for the relation-tuple model.

## Export a portable concept profile

```bash
curl -s -X POST $API/export/profile -H "$KEY" -H "$JSON" \
  -d '{"scope_id":"11111111-1111-1111-1111-111111111111","format":"json"}'
```

## Health and metrics

```bash
curl -s http://localhost:8080/health | jq .
curl -s http://localhost:8080/metrics | head
```

## Further reading

- [../technical/api-reference.md](../technical/api-reference.md) — full reference.
- [build-a-chat-app.md](build-a-chat-app.md) — B2C tutorial.
- [build-b2b-knowledge.md](build-b2b-knowledge.md) — B2B tutorial.
