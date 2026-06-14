# Build a B2B Knowledge Tool (multi-tenant)

An end-to-end walkthrough of building a multi-tenant B2B knowledge tool:
pull knowledge from SaaS sources, isolate tenants, enforce permissions,
and answer questions over it. This is the
[hybrid or enterprise](../product/deployment-scenarios.md) mode.

## What you'll build

A team knowledge service that:

- Connects Notion, Slack, Google Drive (and more) per tenant.
- Keeps each tenant's data isolated.
- Projects source-system ACLs into the permission graph so users only
  see what they're allowed to.
- Synthesizes answers over the ingested content.

## 1. Stand up the gateway

Follow [Quickstart Mode 3](../QUICKSTART.md#mode-3-enterprise-full-stack-multi-tenant-all-connectors)
(or Mode 2 for a lighter SME setup). Set `KNOWLEDGE_API_KEY` /
`KNOWLEDGE_JWT_SECRET` and the Postgres/NATS/MinIO backing services.

## 2. Create a tenant

```bash
API=http://localhost:8080/api/v1
KEY="Authorization: Bearer $KNOWLEDGE_API_KEY"
TID=$(curl -s -X POST $API/tenants -H "$KEY" -H "Content-Type: application/json" \
  -d '{"name":"Acme"}' | jq -r .id)
```

Each tenant is isolated and can carry its own key (rotate via
`/tenants/{id}/key/rotate`).

## 3. Provision users (SCIM)

Wire your IdP to the SCIM v2 endpoints (`/scim/v2/Users`,
`/scim/v2/Groups`) so users and groups are provisioned and deprovisioned
automatically. See
[../technical/api-reference.md](../technical/api-reference.md#scim-v2-identity-provisioning).

## 4. Connect sources

Register a connector per source and run the OAuth2 flow (see
[api-cookbook.md](api-cookbook.md#connect-a-saas-source-oauth2)). On
sync, the connector fetches real document bodies and ingests them into
the tenant's scopes. The connector's `AclSyncEngine` projects the
source's ACLs into the permission graph.

## 5. Enforce permissions

Authorization is a reachability query over relation tuples. Grant and
check access:

```bash
curl -s -X POST $API/permission/grant -H "$KEY" -H "Content-Type: application/json" -d '{
  "object_type":"Domain","object_id":"d-9",
  "relation":"editor",
  "subject_type":"User","subject_id":"u-42"
}'
```

See [../technical/permission-model.md](../technical/permission-model.md)
for namespace inheritance and userset rewrites.

## 6. Query and synthesize per tenant

Ingest/query/synthesize against the tenant's scopes exactly as in the
[API cookbook](api-cookbook.md). Server-side synthesis can run in a TEE
for enterprise deployments.

## 7. Audit

Every privileged action lands in the audit log:

```bash
curl -s "$API/audit?tenant_id=$TID&limit=50" -H "$KEY"
```

## 8. Verify isolation

- Confirm a user in tenant A cannot read tenant B's scopes.
- Confirm a SCIM-deprovisioned user loses access.
- Confirm permission checks match source-system ACLs.

## Operate it

Take it to production with the operator docs:
[deployment-guide.md](../operator/deployment-guide.md),
[monitoring.md](../operator/monitoring.md),
[scaling.md](../operator/scaling.md), and
[backup-recovery.md](../operator/backup-recovery.md).

## What's next

- [comparison.md](../product/comparison.md) — positioning vs. enterprise
  assistants (Glean, Copilot, Dust), vector DBs (Pinecone, Weaviate), and
  hosted memory layers (Mem0, Zep, Letta).
- [ha-failover.md](../operator/ha-failover.md) — active-passive failover
  for the hybrid/enterprise substrate tier (RPO = 0 for acknowledged WAL
  frames, RTO ≤ 2 × lease TTL).
- [add-a-connector.md](add-a-connector.md) — add a source we don't ship.
