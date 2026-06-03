# Operator Guide

This guide covers deploying, monitoring, and operating the Knowledge
stack in production.

## Architecture overview

```
┌────────────┐   HTTP   ┌──────────────────┐  loopback  ┌────────────────────┐
│  Clients   │ ──────── │  Go API Gateway  │ ────────── │  Rust Substrate    │
│            │  :8080   │  (knowledge-gw)  │   :9090    │  (substrate_server)│
└────────────┘          └──────┬───────────┘            └────────────────────┘
                               │
              ┌────────────────┼────────────────┐
              ▼                ▼                ▼
        ┌──────────┐    ┌──────────┐    ┌──────────┐
        │ Postgres │    │   NATS   │    │  MinIO   │
        │ (pg16)   │    │   (JS)   │    │          │
        └──────────┘    └──────────┘    └──────────┘

      ┌────────────────┐           ┌────────────┐
      │  llama-server  │           │ Prometheus │──── Grafana (:3000)
      │  (Bonsai 1.7B) │           │   (:9091)  │
      └────────────────┘           └────────────┘
```

## Deployment

### Prerequisites

- Docker Engine ≥ 24 with Compose v2
- At least 4 GB RAM (8 GB recommended with llama-server)
- The Bonsai-1.7B GGUF model file (for llama-server)

### Quick start

```bash
# 1. Copy the env template and fill in secrets.
cp .env.example .env
# Generate a master key:
openssl rand -hex 32  # paste into KNOWLEDGE_MASTER_KEY

# 2. (Optional) Place the model file for llama-server.
mkdir -p deploy/models
# Download or copy bonsai-1.7b.gguf into deploy/models/

# 3. Start the stack.
make up
# Or: docker compose -f deploy/docker-compose.yml up --build -d
```

### Services

| Service              | Image / Build                    | Port  | Purpose                              |
|----------------------|----------------------------------|-------|--------------------------------------|
| knowledge-gateway    | `deploy/Dockerfile.gateway`      | 8080  | Public API gateway (Go)              |
| knowledge-substrate  | `deploy/Dockerfile.substrate`    | 9090  | Encrypted storage loopback (Rust)    |
| postgres             | `pgvector/pgvector:pg16`         | 5432  | Relational store + pgvector          |
| nats                 | `nats:latest`                    | 4222  | JetStream event bus                  |
| minio                | `minio/minio:latest`             | 9000  | S3-compatible object store           |
| llama-server         | `deploy/Dockerfile.llama-server` | 8081  | On-device SLM inference              |
| prometheus           | `prom/prometheus:latest`         | 9091  | Metrics collection                   |
| grafana              | `grafana/grafana:latest`         | 3000  | Dashboards and alerting              |

## Environment variables

All variables with defaults are listed in `.env.example`. Key
variables:

### Substrate (Rust)

| Variable                   | Required | Default                    | Description                        |
|----------------------------|----------|----------------------------|------------------------------------|
| `KNOWLEDGE_MASTER_KEY`     | **Yes**  | —                          | 64-hex-char SQLCipher master key   |
| `KNOWLEDGE_SUBSTRATE_ADDR` | No       | `127.0.0.1:9090`          | Loopback bind address              |
| `KNOWLEDGE_STORE_PATH`     | No       | `/var/lib/knowledge/substrate.db` | SQLCipher DB path           |

### Gateway (Go)

| Variable                    | Required | Default                    | Description                        |
|-----------------------------|----------|----------------------------|------------------------------------|
| `KNOWLEDGE_API_KEY`         | No       | (empty — auth disabled)    | Static bearer token                |
| `KNOWLEDGE_JWT_SECRET`      | No       | (empty — JWT disabled)     | HMAC secret for tenant JWTs        |
| `KNOWLEDGE_GATEWAY_ADDR`    | No       | `:8080`                    | Public bind address                |
| `KNOWLEDGE_SUBSTRATE_URL`   | No       | `http://127.0.0.1:9090`   | Substrate loopback URL             |
| `KNOWLEDGE_DATABASE_URL`    | No       | (empty — in-memory store)  | Postgres connection string         |
| `KNOWLEDGE_NATS_URL`        | No       | (empty — audit disabled)   | NATS JetStream URL                 |
| `KNOWLEDGE_RATE_IP_RPS`     | No       | `50`                       | Per-IP request-per-second limit    |
| `KNOWLEDGE_RATE_TENANT_RPS` | No       | `200`                      | Per-tenant request-per-second limit|
| `KNOWLEDGE_RATE_BURST`      | No       | `100`                      | Token-bucket burst size            |
| `KNOWLEDGE_CORS_ORIGINS`    | No       | `*`                        | Comma-separated allow-list         |
| `KNOWLEDGE_SYNC_INTERVAL`   | No       | `15m`                      | Default connector sync cadence     |
| `KNOWLEDGE_PUBLIC_BASE_URL` | No       | `http://127.0.0.1:8080`   | Externally reachable base URL      |

### Postgres

| Variable            | Default      |
|---------------------|--------------|
| `POSTGRES_USER`     | `knowledge`  |
| `POSTGRES_PASSWORD` | `knowledge`  |
| `POSTGRES_DB`       | `knowledge`  |

### MinIO

| Variable              | Default       |
|-----------------------|---------------|
| `MINIO_ROOT_USER`     | `minioadmin`  |
| `MINIO_ROOT_PASSWORD` | `minioadmin`  |
| `MINIO_BUCKET`        | `knowledge`   |

### Grafana

| Variable            | Default |
|---------------------|---------|
| `GF_ADMIN_USER`     | `admin` |
| `GF_ADMIN_PASSWORD` | `admin` |

## Monitoring

### Grafana

Open `http://localhost:3000` (default credentials: `admin`/`admin`).
The provisioned **Knowledge Overview** dashboard includes:

| Panel                        | What it shows                                |
|------------------------------|----------------------------------------------|
| Ingest Rate                  | `rate(knowledge_ingest_total[5m])`           |
| Query Rate + Latency         | Query ops/s and p50/p95/p99 latency          |
| Synthesis Trigger/Success    | Trigger, success, and throttle rates         |
| Connector Sync by Provider   | Per-provider success/failure rates           |
| Error Breakdown by Kind      | Error rates by `kind` label                  |
| Subsystem Health             | Up/down status for each subsystem            |
| Memory / CPU                 | Go process RSS and CPU usage                 |
| Postgres Pool Stats          | Request throughput by route                  |

### Prometheus

Prometheus is available at `http://localhost:9091`. It scrapes:

- **knowledge-gateway** (`:8080/metrics`) — Go process + gateway counters
- **knowledge-substrate** (`:9090/internal/metrics`) — Rust FFI counters

### Metric names

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
- `knowledge_ingest_total`, `knowledge_query_total`, `knowledge_errors_total{by_kind}`, etc.

## Alerts

Alerts are defined in `deploy/prometheus/alerts.yml`:

| Alert                          | Condition                            | Severity |
|--------------------------------|--------------------------------------|----------|
| `KnowledgeHighErrorRate`       | >5% error rate for 5 minutes         | warning  |
| `KnowledgeSubsystemDown`      | Health gauge == 0 for 2 minutes      | critical |
| `KnowledgeSynthesisBacklog`   | Pending syntheses stuck 30 minutes   | warning  |
| `KnowledgeConnectorFailing`   | 3+ consecutive provider failures     | warning  |
| `KnowledgeHighLatency`        | p99 latency >500ms for 5 minutes     | warning  |
| `KnowledgeDiskPressure`       | Volume usage >80% for 5 minutes      | warning  |

## Troubleshooting

### Gateway returns 502 / connection refused

The substrate_server is not ready yet. Check:
```bash
docker compose -f deploy/docker-compose.yml logs knowledge-substrate
```
Common causes: missing `KNOWLEDGE_MASTER_KEY`, bad store path
permissions.

### Substrate fails to start with "bad master key"

The `KNOWLEDGE_MASTER_KEY` must be exactly 64 lowercase/uppercase hex
characters. Generate a fresh one:
```bash
openssl rand -hex 32
```

### Postgres migrations fail

The gateway auto-migrates on startup. If it fails:
1. Check Postgres is reachable: `docker exec -it <pg_container> psql -U knowledge`
2. Check the pgvector extension: `SELECT * FROM pg_extension WHERE extname = 'vector';`

### NATS JetStream not available

Verify JetStream is enabled by hitting the monitoring endpoint:
```bash
curl http://localhost:8222/jsz
```

### High memory usage

- The llama-server loads the full GGUF model into RAM. Reduce
  `--ctx-size` or use a smaller quantisation.
- The substrate_server's SQLCipher cache size defaults are tuned for
  moderate workloads; set `PRAGMA cache_size` via the master key
  config if needed.

### Alert: KnowledgeSubsystemDown

A subsystem health gauge dropped to 0. Check the gateway `/health`
endpoint:
```bash
curl -s http://localhost:8080/health | jq .
```
This returns per-subsystem status. The affected subsystem's logs will
have the root cause.

### Alert: KnowledgeConnectorFailing

A connector provider has failed 3+ times consecutively. Check:
```bash
curl -s -H "Authorization: Bearer $KNOWLEDGE_API_KEY" \
  http://localhost:8080/api/v1/connectors | jq '.[] | {id, kind, status}'
```
Common causes: expired OAuth tokens, rate limiting by the upstream
provider, network issues.

## Backup

### Postgres

```bash
# Full dump (run from host or a sidecar with pg_dump):
docker exec <pg_container> pg_dump -U knowledge knowledge > backup.sql

# Restore:
docker exec -i <pg_container> psql -U knowledge knowledge < backup.sql
```

### MinIO

```bash
# Sync the knowledge bucket to a local directory:
mc alias set local http://localhost:9000 minioadmin minioadmin
mc mirror local/knowledge ./minio-backup/
```

### SQLCipher (substrate)

The substrate database is a single SQLCipher file on the
`substrate-data` volume. To back up:
```bash
docker cp <substrate_container>:/data/substrate.db ./substrate-backup.db
```
The backup is encrypted at rest; the `KNOWLEDGE_MASTER_KEY` is
required to open it.

## Upgrade

### Rolling upgrade procedure

1. Pull the latest code and rebuild images:
   ```bash
   git pull origin main
   make up   # rebuilds and recreates changed containers
   ```
2. Postgres migrations run automatically on gateway startup.
3. Monitor the Grafana dashboard for errors during rollout.

### Version compatibility

- The Go gateway and Rust substrate must be deployed from the same
  commit. The loopback API is internal and may change between
  versions.
- Postgres schema migrations are forward-only and additive.
- NATS JetStream streams are created on first use; no manual stream
  management is required.

## Scaling

### Horizontal scaling

- **Gateway**: Run multiple gateway instances behind a load balancer.
  All instances share the same Postgres and NATS cluster. Session
  affinity is not required.
- **Substrate**: Each substrate instance maintains its own local
  SQLCipher store. Scale vertically (more CPU/RAM) rather than
  horizontally. For multi-node deployments, each node gets its own
  substrate instance.

### Vertical tuning

- **Postgres**: Increase `shared_buffers`, `work_mem`, and
  `max_connections` in `postgresql.conf` for higher concurrency.
- **NATS**: Increase the JetStream storage limit with `--store_dir`
  and `--max_mem`.
- **Gateway**: Tune `KNOWLEDGE_RATE_IP_RPS` and
  `KNOWLEDGE_RATE_TENANT_RPS` based on expected traffic.

## Security

### TLS termination

The gateway listens on plain HTTP. In production, terminate TLS at a
reverse proxy (nginx, Caddy, or a cloud load balancer) in front of
the gateway.

### Network segmentation

- The substrate_server binds to loopback (`127.0.0.1:9090`) by
  default. In the Docker network it is accessible only to the gateway.
  Never expose port 9090 to the public internet.
- Postgres, NATS, and MinIO should only be reachable from the
  application network. Do not publish their ports in production
  (remove the `ports:` mappings from `docker-compose.yml`).

### Secret rotation

- **Master key**: Rotating the `KNOWLEDGE_MASTER_KEY` requires
  re-encrypting the substrate database. Export all data, create a new
  store with a fresh key, and re-import.
- **API key / JWT secret**: Update the environment variable and
  restart the gateway. Existing sessions using the old key will be
  rejected.
- **Postgres password**: Update both the Postgres container
  environment and the gateway's `KNOWLEDGE_DATABASE_URL`.
- **MinIO credentials**: Update both the MinIO container environment
  and any client using the S3 API.
