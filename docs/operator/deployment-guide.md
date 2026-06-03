# Deployment Guide

This guide covers deploying the Knowledge stack in production: the
topology, prerequisites, configuration, and a go-live checklist. For
day-2 operations see the companion docs:
[monitoring](monitoring.md), [scaling](scaling.md),
[backup & recovery](backup-recovery.md),
[troubleshooting](troubleshooting.md), and the
[incident runbook](runbook.md).

## Production deployment checklist

- [ ] Generate a strong `KNOWLEDGE_MASTER_KEY` (`openssl rand -hex 32`) and store it in a secret manager — see [key-management.md](../security/key-management.md).
- [ ] Set `KNOWLEDGE_API_KEY` and/or `KNOWLEDGE_JWT_SECRET` (auth is disabled when empty).
- [ ] Terminate TLS at a reverse proxy in front of the gateway.
- [ ] Remove `ports:` mappings for Postgres/NATS/MinIO so they are not publicly reachable.
- [ ] Change all default credentials (Postgres, MinIO, Grafana).
- [ ] Wire [monitoring](monitoring.md) and load the alert rules.
- [ ] Validate a [backup & recovery](backup-recovery.md) drill.

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

## Monitoring & alerting

Grafana dashboards, the Prometheus metric catalogue, the health
endpoint, and the alert rules are documented in
[monitoring.md](monitoring.md). When an alert fires, follow the
[incident runbook](runbook.md).

## Troubleshooting

Common startup and runtime issues (502s, bad master key, migration
failures, JetStream, high memory, connector failures) are covered in
[troubleshooting.md](troubleshooting.md).

## Backup & recovery

Backup strategies for Postgres, MinIO, and the SQLCipher substrate,
plus key escrow and disaster-recovery drills, live in
[backup-recovery.md](backup-recovery.md).

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

Horizontal scaling of the gateway, vertical tuning of the substrate and
stateful tier, and multi-region considerations are covered in
[scaling.md](scaling.md).

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
