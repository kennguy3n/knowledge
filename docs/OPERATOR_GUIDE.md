# Knowledge Platform — Operator Guide

This guide covers deployment, configuration, monitoring, troubleshooting,
backup, upgrade, scaling, and security for the Knowledge Platform.

---

## Table of Contents

1. [Prerequisites](#prerequisites)
2. [Deployment](#deployment)
3. [Configuration](#configuration)
4. [Monitoring](#monitoring)
5. [Alerting](#alerting)
6. [Troubleshooting](#troubleshooting)
7. [Backup & Restore](#backup--restore)
8. [Upgrade Procedure](#upgrade-procedure)
9. [Scaling](#scaling)
10. [Security](#security)

---

## Prerequisites

| Tool             | Minimum Version |
| ---------------- | --------------- |
| Docker           | 24.0+           |
| Docker Compose   | 2.20+           |
| Git              | 2.30+           |
| (optional) Make  | 4.0+            |

Hardware recommendations for a single-node deployment:

| Resource | Development  | Production        |
| -------- | ------------ | ----------------- |
| CPU      | 4 cores      | 8+ cores          |
| RAM      | 8 GB         | 16+ GB            |
| Disk     | 20 GB SSD    | 100+ GB NVMe SSD  |
| GPU      | —            | Optional (llama)  |

---

## Deployment

### 1. Clone the repository

```bash
git clone https://github.com/kennguy3n/knowledge.git
cd knowledge
```

### 2. Configure environment variables

```bash
cp deploy/.env.example deploy/.env
# Edit deploy/.env with production values — at minimum:
#   KNOWLEDGE_API_KEY, POSTGRES_PASSWORD, KNOWLEDGE_MASTER_KEY,
#   MINIO_SECRET_KEY, GRAFANA_ADMIN_PASSWORD
```

### 3. Prepare the model weight file

Download or copy the Bonsai-1.7B GGUF weight file:

```bash
mkdir -p deploy/models
# Place bonsai-1.7b-q1_0.gguf into deploy/models/
```

### 4. Start all services

```bash
make up
# — or directly —
docker compose -f deploy/docker-compose.yml up --build -d
```

### 5. Verify health

```bash
# Gateway health
curl http://localhost:8080/healthz

# Substrate health
curl http://localhost:9090/substrate/health

# Prometheus targets
curl http://localhost:9091/api/v1/targets

# Grafana
open http://localhost:3000   # admin / (your GRAFANA_ADMIN_PASSWORD)
```

### 6. Stop services

```bash
make down
```

---

## Configuration

All services are configured via environment variables. Reference
[`deploy/.env.example`](../deploy/.env.example) for the full list.

### Gateway (`knowledge-gateway`)

| Variable              | Default                                   | Description                              |
| --------------------- | ----------------------------------------- | ---------------------------------------- |
| `KNOWLEDGE_API_KEY`   | `changeme`                                | API key for authenticating clients       |
| `DATABASE_URL`        | `postgres://knowledge:knowledge@postgres:5432/knowledge?sslmode=disable` | PostgreSQL connection string |
| `NATS_URL`            | `nats://nats:4222`                        | NATS server URL                          |
| `MINIO_ENDPOINT`      | `minio:9000`                              | MinIO S3-compatible endpoint             |
| `MINIO_ACCESS_KEY`    | `minioadmin`                              | MinIO access key                         |
| `MINIO_SECRET_KEY`    | `minioadmin`                              | MinIO secret key                         |
| `SUBSTRATE_URL`       | `http://knowledge-substrate:9090`         | Substrate server URL (internal)          |

### Substrate (`knowledge-substrate`)

| Variable                | Default                              | Description                           |
| ----------------------- | ------------------------------------ | ------------------------------------- |
| `KNOWLEDGE_MASTER_KEY`  | (hex string)                         | 256-bit master encryption key (hex)   |
| `LLAMA_SERVER_URL`      | `http://llama-server:8081`           | llama.cpp server URL                  |
| `SUBSTRATE_LISTEN_ADDR` | `0.0.0.0:9090`                       | Listen address                        |

### PostgreSQL

| Variable            | Default      | Description        |
| ------------------- | ------------ | ------------------ |
| `POSTGRES_USER`     | `knowledge`  | Database user      |
| `POSTGRES_PASSWORD` | `knowledge`  | Database password  |
| `POSTGRES_DB`       | `knowledge`  | Database name      |

### MinIO

| Variable           | Default        | Description       |
| ------------------ | -------------- | ----------------- |
| `MINIO_ACCESS_KEY` | `minioadmin`   | Root access key   |
| `MINIO_SECRET_KEY` | `minioadmin`   | Root secret key   |

### Grafana

| Variable                 | Default  | Description            |
| ------------------------ | -------- | ---------------------- |
| `GRAFANA_ADMIN_USER`     | `admin`  | Admin username         |
| `GRAFANA_ADMIN_PASSWORD` | `admin`  | Admin password         |

---

## Monitoring

### Endpoints

| Service      | URL                          | Purpose                  |
| ------------ | ---------------------------- | ------------------------ |
| Grafana      | `http://localhost:3000`      | Dashboards & alerting    |
| Prometheus   | `http://localhost:9091`      | Metrics & alerting rules |
| Gateway      | `http://localhost:8080`      | API gateway              |
| Substrate    | `http://localhost:9090`      | Rust substrate server    |
| MinIO Console| `http://localhost:9001`      | Object storage UI        |
| NATS Monitor | `http://localhost:8222`      | NATS monitoring          |

### Knowledge Platform Overview Dashboard

The provisioned Grafana dashboard (`Knowledge Platform Overview`) contains
these panels:

| Panel                       | Metric(s)                                          | What to watch                                     |
| --------------------------- | -------------------------------------------------- | ------------------------------------------------- |
| **Ingest Rate**             | `rate(knowledge_ingest_total[5m])`                 | Sustained throughput; sudden drops = ingestion issue |
| **Query Rate**              | `rate(knowledge_query_total[5m])`                  | Traffic pattern; spikes may indicate runaway clients |
| **Query Latency**           | `knowledge_query_duration_seconds_bucket` p50/p90/p99 | p99 > 500ms triggers alert                      |
| **Synthesis Rates**         | `knowledge_synthesis_triggered_total`, `trigger_server_synthesis_total`, throttled | Throttle rate > 0 = pipeline bottleneck |
| **Connector Sync Status**   | `sync_connector_total`, scheduler dispatches       | Failed > 0 needs investigation                    |
| **Error Breakdown by Kind** | `knowledge_errors_by_kind`                         | Stacked bar — identify dominant error category     |
| **Subsystem Health**        | `knowledge_subsystem_status`                       | Any subsystem at 0 = critical                      |
| **Memory & CPU**            | `process_resident_memory_bytes`, `process_cpu_seconds_total` | Detect memory leaks and CPU saturation    |
| **PostgreSQL Pool**         | `knowledge_db_pool_{active,idle,max}_connections`  | Active near max = pool exhaustion risk             |

---

## Alerting

Prometheus alerting rules are defined in
[`deploy/prometheus/alerts.yml`](../deploy/prometheus/alerts.yml).

### KnowledgeHighErrorRate

- **Condition**: Error rate > 5% of combined ingest + query traffic for 5 minutes.
- **Severity**: Critical.
- **Action**: Check `knowledge_errors_by_kind` for the dominant error type.
  Common causes: database connection issues (`storage`), encryption failures
  (`crypto`), or upstream dependency timeouts (`unavailable`).

### KnowledgeSubsystemDown

- **Condition**: Any subsystem reports status `0` (Unavailable) for 2 minutes.
- **Severity**: Critical.
- **Action**: Check substrate logs (`make logs`) for the specific subsystem.
  The `bridge` subsystem failing usually indicates the FFI layer cannot load.
  The `evidence_store` subsystem failing indicates SQLCipher / database issues.
  The `inference_router` subsystem failing indicates the llama server is
  unreachable.

### KnowledgeSynthesisBacklog

- **Condition**: Stuck pending synthesis windows for 30 minutes.
- **Severity**: Warning.
- **Action**: Check if the llama server is healthy
  (`curl http://localhost:8081/health`). Verify the synthesis pipeline
  configuration. Retry stuck windows via the trigger endpoint.

### KnowledgeConnectorFailing

- **Condition**: 3+ sync failures in 15 minutes.
- **Severity**: Warning.
- **Action**: Check the connector's upstream provider status. Verify OAuth
  tokens have not expired. Review connector logs for specific error messages.
  Use `refresh_connector_token` to force a token refresh if needed.

### KnowledgeHighLatency

- **Condition**: p99 query latency > 500ms for 5 minutes.
- **Severity**: Warning.
- **Action**: Check PostgreSQL query performance (`pg_stat_activity`).
  Verify FTS5 indexes are healthy. Check system resources (CPU, memory, disk
  I/O). Consider adding indexes for frequently-queried fields.

### KnowledgeDiskPressure

- **Condition**: Any volume exceeds 80% capacity.
- **Severity**: Warning.
- **Action**: Expand Docker volumes or the underlying disk. Prune old data:
  - PostgreSQL: Archive or delete old evidence rows.
  - MinIO: Remove stale objects with lifecycle policies.
  - NATS: Verify JetStream stream limits are configured.

---

## Troubleshooting

### Container won't start

```bash
# Check container status
docker compose -f deploy/docker-compose.yml ps

# View logs for a specific service
docker compose -f deploy/docker-compose.yml logs <service-name>

# Common issues:
# - Port already in use: stop conflicting services or change port mapping
# - Missing model file: ensure bonsai-1.7b-q1_0.gguf is in deploy/models/
# - Permission denied: check file ownership on mounted volumes
```

### Substrate build fails

The Rust substrate build requires `build-essential`, `libssl-dev`, and
`pkg-config` for SQLCipher's vendored OpenSSL compile. These are included
in the Dockerfile. If the build still fails:

```bash
# Rebuild without cache
docker compose -f deploy/docker-compose.yml build --no-cache knowledge-substrate
```

### Database connection refused

```bash
# Verify PostgreSQL is healthy
docker compose -f deploy/docker-compose.yml exec postgres pg_isready

# Check connection string matches env vars
docker compose -f deploy/docker-compose.yml exec knowledge-gateway env | grep DATABASE_URL
```

### llama-server OOM or slow startup

The llama-server may take 30–60 seconds to load the model. If it runs
out of memory:

- Ensure the host has at least 4 GB free RAM for Bonsai-1.7B.
- Consider a smaller quantization if available.
- Check `docker stats` for memory usage.

### NATS JetStream not initialising

```bash
# Verify JetStream is enabled
docker compose -f deploy/docker-compose.yml exec nats nats-server --help | grep jetstream

# Check NATS monitoring
curl http://localhost:8222/jsz
```

---

## Backup & Restore

### PostgreSQL

Schedule regular `pg_dump` backups:

```bash
# Full backup
docker compose -f deploy/docker-compose.yml exec postgres \
  pg_dump -U knowledge -Fc knowledge > backup_$(date +%Y%m%d_%H%M%S).dump

# Restore
docker compose -f deploy/docker-compose.yml exec -T postgres \
  pg_restore -U knowledge -d knowledge --clean < backup.dump
```

**Recommended schedule**: Daily full backup + WAL archiving for point-in-time
recovery in production.

### MinIO

Sync MinIO buckets to an external backup location:

```bash
# Using mc (MinIO Client)
mc alias set local http://localhost:9000 minioadmin minioadmin
mc mirror local/knowledge-bucket /backup/minio/knowledge-bucket
```

### SQLCipher Databases

The substrate stores per-scope encrypted databases. To back up:

```bash
# Copy the data directory (databases are self-contained files)
docker compose -f deploy/docker-compose.yml exec knowledge-substrate \
  cp -r /data/scopes /backup/scopes_$(date +%Y%m%d)
```

> **Important**: SQLCipher databases are encrypted at rest. The
> `KNOWLEDGE_MASTER_KEY` is required to decrypt them. Store the master
> key separately from the backup using a secrets manager.

### NATS JetStream

JetStream data persists in the `nats_data` volume. Back up the volume:

```bash
docker run --rm -v knowledge_nats_data:/data -v $(pwd):/backup \
  alpine tar czf /backup/nats_backup_$(date +%Y%m%d).tar.gz /data
```

---

## Upgrade Procedure

### 1. Pre-flight checks

```bash
# Check current versions
docker compose -f deploy/docker-compose.yml exec knowledge-gateway \
  /knowledge-gateway --version 2>/dev/null || echo "check gateway version"

docker compose -f deploy/docker-compose.yml exec knowledge-substrate \
  substrate_server --version 2>/dev/null || echo "check substrate version"

# Back up PostgreSQL
docker compose -f deploy/docker-compose.yml exec postgres \
  pg_dump -U knowledge -Fc knowledge > pre_upgrade_backup.dump
```

### 2. Pull latest code

```bash
git pull origin main
```

### 3. Run migrations

```bash
make migrate
# Or run any pending migration scripts in migrations/ directory
```

### 4. Rebuild and restart

```bash
make down
make up
```

### 5. Verify

```bash
# Health checks
curl http://localhost:8080/healthz
curl http://localhost:9090/substrate/health

# Check Grafana dashboards for anomalies
open http://localhost:3000
```

### Version Compatibility

| Gateway | Substrate | Notes                                  |
| ------- | --------- | -------------------------------------- |
| v1.x    | v1.x      | Initial release                        |

> The substrate's `MetricsSnapshot` wire format is additive-only — new
> fields default to zero. Older gateways can safely talk to newer
> substrates (new fields are ignored). A newer gateway talking to an
> older substrate will see `0` for any fields the substrate doesn't yet
> emit.

---

## Scaling

### Horizontal Scaling (Gateway)

The gateway is stateless and can be scaled behind a load balancer:

```yaml
# docker-compose.override.yml
services:
  knowledge-gateway:
    deploy:
      replicas: 3
```

Use an external load balancer (Nginx, HAProxy, or cloud ALB) to
distribute traffic across gateway instances. All instances share the
same PostgreSQL, NATS, and MinIO backends.

### Vertical Scaling

| Component         | Tuning Parameter                              | Default | Notes                          |
| ----------------- | --------------------------------------------- | ------- | ------------------------------ |
| PostgreSQL        | `max_connections`                             | 100     | Increase for higher concurrency |
| PostgreSQL        | `shared_buffers`                              | 128 MB  | Set to 25% of available RAM    |
| PostgreSQL        | `work_mem`                                    | 4 MB    | Increase for complex queries   |
| Gateway           | `GOMAXPROCS`                                  | (auto)  | Set to CPU core count          |
| Substrate         | `SUBSTRATE_LISTEN_ADDR`                       | 0.0.0.0:9090 | One instance per host     |
| llama-server      | `--threads` / `--ctx-size`                    | (auto)  | Match to available CPU/RAM     |
| NATS              | `max_payload` / `max_connections`             | 1 MB / 65536 | Increase for heavy workloads |

### Scaling Substrate

The substrate server is stateful (SQLCipher databases are local). To
scale horizontally:

1. Use a shared filesystem (NFS, EFS) for the data directory, or
2. Shard by scope ID — route each scope to a dedicated substrate instance.

---

## Security

### TLS Termination

In production, terminate TLS at a reverse proxy in front of the gateway:

```nginx
# nginx.conf snippet
server {
    listen 443 ssl;
    ssl_certificate     /etc/ssl/certs/knowledge.crt;
    ssl_certificate_key /etc/ssl/private/knowledge.key;

    location / {
        proxy_pass http://knowledge-gateway:8080;
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
    }
}
```

### Network Segmentation

The `knowledge-net` Docker network isolates all services. In
production:

- Only expose the gateway port (8080/443) to the external network.
- Keep Prometheus (9091), Grafana (3000), and all backend services on
  an internal-only network.
- Use Docker network policies or firewall rules to restrict inter-service
  communication to the minimum required.

### Secret Rotation

| Secret                 | Rotation Frequency | Procedure                                                |
| ---------------------- | ------------------ | -------------------------------------------------------- |
| `KNOWLEDGE_API_KEY`    | 90 days            | Update gateway env var, restart gateway                  |
| `KNOWLEDGE_MASTER_KEY` | **Never rotate**   | Tied to encrypted data — rotating requires re-encryption |
| `POSTGRES_PASSWORD`    | 90 days            | `ALTER ROLE` in PostgreSQL, update all consumers         |
| `MINIO_SECRET_KEY`     | 90 days            | Update MinIO config, update all consumers                |
| `GRAFANA_ADMIN_PASSWORD`| 90 days           | Change via Grafana UI or API                             |

> **Warning**: The `KNOWLEDGE_MASTER_KEY` encrypts all SQLCipher
> databases. Losing this key means losing all encrypted data. Store it
> in a hardware-backed secrets manager (AWS KMS, HashiCorp Vault, etc.)
> and never commit it to version control.

### Additional Recommendations

- Enable PostgreSQL SSL (`sslmode=require` in `DATABASE_URL`).
- Run containers as non-root users where possible.
- Use read-only filesystem mounts for configuration files (already set
  via `:ro` in docker-compose.yml).
- Enable Docker Content Trust for image signing.
- Scan images for vulnerabilities with Trivy or Snyk.
- Monitor `docker events` for unauthorized container operations.
