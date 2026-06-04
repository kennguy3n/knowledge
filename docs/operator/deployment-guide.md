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

### Deploy with pre-built images

Tagged releases publish multi-arch (amd64/arm64) `gateway` and
`substrate` images to GHCR via the
[`Publish images`](../../.github/workflows/docker-publish.yml) workflow,
so SMEs can deploy with `docker pull` + `docker compose up` and **no
local build**.

```bash
# Pull the published images (replace 0.1.0 with the release tag).
export KNOWLEDGE_VERSION=0.1.0
docker pull ghcr.io/kennguy3n/knowledge-gateway:${KNOWLEDGE_VERSION}
docker pull ghcr.io/kennguy3n/knowledge-substrate:${KNOWLEDGE_VERSION}
```

To run the compose stack against the pre-built images instead of
building locally, point the two core services at the published tags with
a compose override file:

```yaml
# deploy/docker-compose.images.yml
services:
  knowledge-gateway:
    image: ghcr.io/kennguy3n/knowledge-gateway:${KNOWLEDGE_VERSION:-latest}
    build: !reset null
  knowledge-substrate:
    image: ghcr.io/kennguy3n/knowledge-substrate:${KNOWLEDGE_VERSION:-latest}
    build: !reset null
```

```bash
cp .env.example .env   # set KNOWLEDGE_MASTER_KEY (openssl rand -hex 32)
docker compose \
  -f deploy/docker-compose.yml \
  -f deploy/docker-compose.images.yml \
  up -d   # no --build: images are pulled, not compiled
```

> **Requires Docker Compose v2.24+.** The override uses the `!reset null`
> tag to drop the inherited `build:` block; older Compose versions fail to
> parse it with a cryptic YAML tag error. Check with `docker compose version`.
> On an older Compose, either upgrade or delete the two `build:` lines from
> the override file instead of using `!reset`.

The `llama-server` image is **not** published (it is a large, optional
on-device inference component); build it locally if needed, or omit it.

Images are also pushed to Docker Hub when the repository defines the
`DOCKERHUB_USERNAME` / `DOCKERHUB_TOKEN` secrets — substitute
`docker.io/<username>/knowledge-gateway` for the GHCR reference.

### Deploy on Kubernetes (Helm)

A Helm chart at [`deploy/helm/knowledge`](../../deploy/helm/knowledge)
mirrors the compose topology: a horizontally-scalable gateway Deployment
in front of a single stateful substrate Deployment backed by a
`PersistentVolumeClaim` for the SQLCipher database.

```bash
# Generate and pass the SQLCipher master key (required).
helm install knowledge deploy/helm/knowledge \
  --namespace knowledge --create-namespace \
  --set secrets.masterKey="$(openssl rand -hex 32)"
```

Common overrides (see
[`values.yaml`](../../deploy/helm/knowledge/values.yaml) for the full
list):

| Value                                  | Default                                   | Purpose                                   |
|----------------------------------------|-------------------------------------------|-------------------------------------------|
| `gateway.image.tag` / `substrate.image.tag` | chart `appVersion`                   | Pin the published image tag.              |
| `gateway.replicaCount`                 | `2`                                       | Static gateway replicas (when HPA is off).|
| `autoscaling.enabled`                  | `false`                                   | Enable the gateway HPA.                   |
| `substrate.persistence.size`           | `10Gi`                                    | SQLCipher volume size.                    |
| `substrate.persistence.storageClass`   | `""` (cluster default)                    | Block-storage class for the substrate PVC.|
| `config.databaseUrl`                   | `""`                                      | External Postgres DSN (else in-memory).   |
| `config.natsUrl`                       | `""`                                      | External NATS URL (else audit disabled).  |
| `ingress.enabled` / `ingress.className`| `false` / `""`                            | Expose the gateway via an Ingress.        |
| `secrets.existingSecret`               | `""`                                      | Use a pre-created Secret for the keys.    |

Production notes:

- **Master key durability** — the substrate PVC carries
  `helm.sh/resource-policy: keep`, so `helm uninstall` will **not**
  delete the encrypted store. Losing either the PVC or the master key is
  unrecoverable (see [backup & recovery](backup-recovery.md)).
- **Secrets** — prefer `secrets.existingSecret` (a Secret you manage out
  of band) over `secrets.masterKey` so the key never lands in
  values/CI logs. The chart rejects a `masterKey` that is not 64 hex
  characters.
- **Substrate is single-replica** — it owns the SQLCipher file on a
  `ReadWriteOnce` volume and must not be scaled horizontally; only the
  gateway is autoscaled.
- **TLS** — terminate TLS at the Ingress; the gateway speaks plain HTTP.

Render the manifests without installing to review them:

```bash
helm template knowledge deploy/helm/knowledge \
  --set secrets.masterKey="$(openssl rand -hex 32)" | less
```

#### Provisioning a cluster (Terraform)

Starting-point Terraform modules provision a managed cluster to deploy
the chart onto: [`deploy/terraform/aws`](../../deploy/terraform/aws)
(EKS + EBS CSI) and [`deploy/terraform/gcp`](../../deploy/terraform/gcp)
(GKE Autopilot). They create the cluster only — run `helm install`
afterwards. See each module's README for inputs and hardening notes.

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
