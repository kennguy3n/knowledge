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
- [ ] Set strong Postgres, MinIO, and Grafana passwords — these have **no defaults**; `docker compose` refuses to start until they are set in `.env`.
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
- *(Optional)* a custom GGUF model file, only if you want to override the
  Bonsai-1.7B weights that ship baked into the `llama-server` image

### Quick start

```bash
# 1. Copy the env template and fill in secrets.
cp .env.example .env
# Generate a master key:
openssl rand -hex 32  # paste into KNOWLEDGE_MASTER_KEY

# 2. Start the stack. The llama-server image ships the Bonsai-1.7B GGUF
#    baked in, so synthesis works out of the box — no model download.
make up
# Or: docker compose -f deploy/docker-compose.yml up --build -d
```

> **Overriding the bundled model.** To serve a different GGUF, bind-mount
> it over the baked-in path by uncommenting the `volumes:` override on the
> `llama-server` service in `deploy/docker-compose.yml`:
>
> ```yaml
>     volumes:
>       - "/path/to/custom-model.gguf:/models/bonsai-1.7b.gguf:ro"
> ```

### Deploy with pre-built images

Tagged releases publish multi-arch (amd64/arm64) `gateway`, `substrate`,
and `llama-server` images to GHCR via the
[`Publish images`](../../.github/workflows/docker-publish.yml) workflow,
so SMEs can deploy with `docker pull` + `docker compose up` and **no
local build**. The `llama-server` image ships the Bonsai-1.7B GGUF baked
in, so synthesis works without a separate model download.

```bash
# Pull the published images (replace 1.2.0 with the release tag).
export KNOWLEDGE_VERSION=1.2.0
docker pull ghcr.io/kennguy3n/knowledge-gateway:${KNOWLEDGE_VERSION}
docker pull ghcr.io/kennguy3n/knowledge-substrate:${KNOWLEDGE_VERSION}
docker pull ghcr.io/kennguy3n/knowledge-llama-server:${KNOWLEDGE_VERSION}
```

To run the compose stack against the pre-built images instead of
building locally, point the services at the published tags with a compose
override file (this is exactly
[`deploy/docker-compose.images.yml`](../../deploy/docker-compose.images.yml)):

```yaml
# deploy/docker-compose.images.yml
services:
  knowledge-gateway:
    image: ghcr.io/kennguy3n/knowledge-gateway:${KNOWLEDGE_VERSION:-latest}
    build: !reset null
  knowledge-substrate:
    image: ghcr.io/kennguy3n/knowledge-substrate:${KNOWLEDGE_VERSION:-latest}
    build: !reset null
  llama-server:
    image: ghcr.io/kennguy3n/knowledge-llama-server:${KNOWLEDGE_VERSION:-latest}
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

The `llama-server` image is large (it compiles llama.cpp from source and
bakes in the GGUF weights), but it **is** published like the others, so
operators do not need to build it locally or supply a model file.

Images are also pushed to Docker Hub when the repository defines the
`DOCKERHUB_USERNAME` / `DOCKERHUB_TOKEN` secrets — substitute
`docker.io/<username>/knowledge-gateway` for the GHCR reference.

### Deploy on Kubernetes (Helm)

A Helm chart at [`deploy/helm/knowledge`](../../deploy/helm/knowledge)
mirrors the compose topology: a horizontally-scalable gateway Deployment
in front of a single stateful substrate Deployment backed by a
`PersistentVolumeClaim` for the SQLCipher database, plus a single-replica
`llama-server` Deployment (the SLM sidecar, with the Bonsai-1.7B GGUF
baked into its image). When `llamaServer.enabled` is true (the default),
the substrate is wired to the sidecar automatically, so synthesis works
out of the box; set it to `false` to deploy without server-side
synthesis.

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
| `gateway.image.repository` / `substrate.image.repository` | `ghcr.io/kennguy3n/knowledge-*` | Image repos — **override when deploying from a fork's registry.** |
| `gateway.image.tag` / `substrate.image.tag` | chart `appVersion`                   | Pin the published image tag.              |
| `gateway.replicaCount`                 | `2`                                       | Static gateway replicas (when HPA is off).|
| `autoscaling.enabled`                  | `false`                                   | Enable the gateway HPA.                   |
| `substrate.persistence.size`           | `10Gi`                                    | SQLCipher volume size.                    |
| `llamaServer.enabled`                  | `true`                                    | Deploy the SLM sidecar (Bonsai-1.7B baked in) and wire the substrate to it. |
| `llamaServer.image.repository`         | `ghcr.io/kennguy3n/knowledge-llama-server`| SLM image repo — override for a fork's registry. |
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
  characters. Note that with `existingSecret` the chart no longer owns the
  Secret, so rotating it does **not** auto-restart the pods (the
  `checksum/secret` annotation only tracks the chart-managed Secret) — use
  a controller like [stakater/Reloader](https://github.com/stakater/Reloader),
  or `kubectl rollout restart` the deployments after rotating.
- **Image registry** — the chart defaults to the upstream
  `ghcr.io/kennguy3n/knowledge-*` images. If you publish from a fork (the
  `docker-publish.yml` workflow pushes to *your* `ghcr.io/<owner>`
  namespace), override `gateway.image.repository` and
  `substrate.image.repository` to match. The same applies to the
  `docker-compose.images.yml` overlay.
- **Substrate is single-replica by default** — it owns the SQLCipher
  file on a `ReadWriteOnce` volume and must not be scaled horizontally as
  a `Deployment`. For redundancy use the active-passive HA mode below
  (`substrate.ha.enabled`); only the gateway is autoscaled.
- **TLS** — terminate TLS at the Ingress; the gateway speaks plain HTTP.

#### Active-passive HA (WAL shipping)

Setting `substrate.ha.enabled: true` (with `substrate.ha.replicas: 2`)
replaces the single substrate `Deployment` with a `StatefulSet`: the
pod that wins the NATS leadership lease runs as **primary** (writes in
`journal_mode=WAL` and ships WAL frames over NATS JetStream), the others
run as **standby** (replay the frames read-only and promote on primary
failure). HA requires `config.natsUrl` to be set — the WAL stream and
the leadership lease both ride that NATS connection.

> **⚠️ Migrating an existing install from single-replica to HA is not
> automatic — it will start the primary on an empty database.** The
> standalone `Deployment` mounts a PVC named
> `<release>-substrate-data`, whereas the StatefulSet provisions its
> own per-pod PVCs from a `volumeClaimTemplate` named `data`
> (`data-<release>-substrate-0`, `data-<release>-substrate-1`, …). Pod
> ordinal 0's PVC is a **brand-new, empty** volume, so flipping
> `substrate.ha.enabled` to `true` on a live install does **not** carry
> the existing SQLCipher store across — the new primary would come up
> with no data. Before enabling HA, migrate the existing volume during a
> maintenance window: scale the substrate to zero, then copy the
> SQLCipher file (`substrate.db`) from the old `<release>-substrate-data`
> PVC onto the **per-pod** StatefulSet PVCs (e.g. with a one-shot
> `kubectl` copy Job mounting both claims, or restore from a
> [backup](backup-recovery.md)), then `helm upgrade` with HA enabled.
> Seed **every** ordinal that needs the historical data, not just
> `data-<release>-substrate-0`: replication ships only *incremental* WAL
> frames from the point a node becomes primary, so a standby that starts
> from an empty PVC cannot reconstruct rows written before the cutover —
> it would only ever receive pages changed afterwards. (A genuinely
> *fresh* install is the exception: there pod-0 and the standbys all
> start empty and the standby tails the primary's stream from the very
> first frame, so no seeding is needed.) The old PVC carries
> `helm.sh/resource-policy: keep`, so it is not deleted on upgrade —
> retain it until you have verified the migrated data.

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
| llama-server         | `deploy/Dockerfile.llama-server` | 8081  | On-device SLM inference (Bonsai-1.7B GGUF baked in) |
| prometheus           | `prom/prometheus:latest`         | 9091  | Metrics collection                   |
| grafana              | `grafana/grafana:latest`         | 3000  | Dashboards and alerting              |
| admin                | `admin/Dockerfile` (nginx)       | 3001  | Browser-based admin dashboard        |
| knowledge-ui         | `apps/knowledge-ui/Dockerfile`   | 3002  | End-user reference UI (Next.js)      |

### Admin dashboard

The `admin` service serves a lightweight React SPA at
`http://localhost:3001` (override with `ADMIN_PORT`) that fronts the
gateway's public HTTP surface — no CLI or PromQL required. From it you
can:

- view aggregate **health and headline metrics** (Dashboard),
- **create, sync, re-auth, and delete connector instances** (Connectors),
- manage **tenants**, rotate keys, and view members (Tenants),
- trigger and inspect **synthesis** runs (Synthesis),
- browse decaying **memory** objects by decay state (Memory),
- query the tamper-evident **audit log** (Audit),
- set the gateway URL / bearer token and run cryptographic-forget from
  the danger zone (Settings).

The container is built from `admin/Dockerfile` and reverse-proxies API
calls to the gateway, so it only needs network reachability to
`knowledge-gateway`. See [`admin/README.md`](../../admin/README.md) for
the page-to-endpoint map and local-dev instructions.

### End-user reference UI

The `knowledge-ui` service serves a consumer-facing **Next.js 14** app at
`http://localhost:3002` (override with `UI_PORT`). Unlike the operator
`admin/` dashboard, it targets end users: chat with a scope, run hybrid
search, browse synthesized memory and its decay state, stream synthesis
progress over SSE, and cryptographically forget a conversation. It is a
thin, fully client-side client over the gateway's public REST surface,
shipped as a static export behind nginx (same-origin reverse proxy to the
gateway), so — like `admin` — it only needs network reachability to
`knowledge-gateway`. See
[`apps/knowledge-ui/README.md`](../../apps/knowledge-ui/README.md) for the
page-to-endpoint map and local-dev instructions.

## High availability (active-passive failover)

The substrate stores everything in a single SQLCipher (SQLite) database,
which cannot scale out horizontally — but it **can** be made highly
available through **WAL shipping**. A primary runs in WAL journal mode
and ships each committed transaction's frames over NATS JetStream to one
or more standbys, which replay them into a local shadow WAL and serve
**read-only** queries. Leadership is held via a NATS key-value lease; if
the primary's lease expires, a standby wins the lease and promotes itself
to primary. See [ha-failover.md](ha-failover.md) for the recovery
objectives (RPO = 0 for acked frames; RTO ≤ 2 × lease TTL), the failover
model, and how both are measured.

The replication transport is gated behind the non-default
`replication-nats` cargo feature, so standalone and cross-compile builds
stay lean. Build the substrate image with it enabled to use HA:

```bash
REPLICATION_NATS=1 docker compose -f deploy/docker-compose.yml build knowledge-substrate
```

### Compose (single-host demo)

`deploy/docker-compose.yml` ships a commented-out
`knowledge-substrate-standby` service. To enable active-passive failover
on one host:

1. Build the substrate image with `REPLICATION_NATS=1` (above).
2. Set the primary's role: `KNOWLEDGE_SUBSTRATE_ROLE=primary` (or run
   both nodes as `auto` to let them elect a leader via the NATS KV lock).
3. Uncomment the `knowledge-substrate-standby` service and the
   `substrate-standby-data` volume.
4. Point the gateway at the standby with
   `KNOWLEDGE_SUBSTRATE_URL_STANDBY=http://knowledge-substrate-standby:9090`.

The gateway routes writes to the primary (failing over on a `503`
standby/unreachable response) and offloads reads to a standby.

### Kubernetes (Helm)

Set `substrate.ha.enabled=true` to render the substrate as a
**StatefulSet** (one PVC per pod) instead of the single Deployment. One
pod is primary at a time; the rest are warm standbys. The gateway
addresses pods by their stable StatefulSet DNS names and routes around a
demoted/promoted node.

```bash
helm install knowledge deploy/helm/knowledge \
  --namespace knowledge --create-namespace \
  --set secrets.masterKey="$(openssl rand -hex 32)" \
  --set substrate.ha.enabled=true \
  --set substrate.ha.replicas=2
```

### Configuration & monitoring

| Setting | Where | Purpose |
|---|---|---|
| `KNOWLEDGE_SUBSTRATE_ROLE` | substrate | `primary` / `standby` / `auto` / `disabled` (also `--role`) |
| `KNOWLEDGE_REPLICATION_NATS_URL` | substrate | NATS JetStream URL carrying the WAL stream + leadership lease |
| `KNOWLEDGE_SUBSTRATE_URL_STANDBY` | gateway | Standby substrate URL; enables write failover + read offload |
| `substrate.ha.enabled` / `substrate.ha.replicas` | Helm | Render the StatefulSet and set the replica count |

The substrate's `/health` endpoint gains a `replication` object (`role`,
`lag_frames`, `last_applied_at`, …), and `/internal/metrics` exposes
`knowledge_replication_lag_frames`. The bundled Grafana dashboard adds a
**Substrate Replication Lag (frames)** panel and the Prometheus rules add
a `KnowledgeReplicationLagHigh` alert (fires when a standby is >1000 WAL
frames behind). See [monitoring.md](monitoring.md).

## One-command installer

For SMEs, `scripts/install.sh` (bash) and `scripts/install.ps1`
(PowerShell) take a fresh host from zero to a running stack: they check
Docker + the Compose plugin, generate per-deployment secrets into `.env`
(mode 600, never overwriting an existing file), prompt for on-device
synthesis, start the published-image stack, wait for the gateway to
report healthy, and print the URLs to open.

```bash
# From a clone:
./scripts/install.sh

# Or straight from the web (downloads the compose files into ./knowledge):
curl -fsSL https://raw.githubusercontent.com/kennguy3n/knowledge/main/scripts/install.sh | bash
```

On Windows: `./scripts/install.ps1`, or
`irm https://raw.githubusercontent.com/kennguy3n/knowledge/main/scripts/install.ps1 | iex`.

Both installers honor the same environment overrides (all optional):

| Variable | Purpose |
|---|---|
| `KNOWLEDGE_SLM_DEVICE_TIER` | `high` / `medium` / `low` — skips the synthesis prompt |
| `KNOWLEDGE_ASSUME_YES` | `1` — non-interactive; accept defaults (enables synthesis) |
| `KNOWLEDGE_IMAGE_TAG` | Published image tag to run (default `latest`) |
| `KNOWLEDGE_HOME` | Install dir for the curl-pipe path (default `./knowledge`) |
| `KNOWLEDGE_INSTALL_DRY_RUN` | `1` — do everything except `docker compose up` / the health wait |

The published `llama-server` image ships the Bonsai-1.7B GGUF baked in,
so on-device synthesis works with no manual model download.

## Environment variables

All variables with defaults are listed in `.env.example`. Key
variables:

### Substrate (Rust)

| Variable                   | Required | Default                    | Description                        |
|----------------------------|----------|----------------------------|------------------------------------|
| `KNOWLEDGE_MASTER_KEY`     | **Yes**  | —                          | 64-hex-char SQLCipher master key   |
| `KNOWLEDGE_SUBSTRATE_ADDR` | No       | `127.0.0.1:9090`          | Loopback bind address              |
| `KNOWLEDGE_STORE_PATH`     | No       | `/var/lib/knowledge/substrate.db` | SQLCipher DB path           |
| `KNOWLEDGE_SUBSTRATE_ROLE` | No       | `disabled`                 | HA role: `primary` / `standby` / `auto` / `disabled` (also `--role`) |
| `KNOWLEDGE_REPLICATION_NATS_URL` | No | `nats://nats:4222`         | NATS JetStream URL carrying the WAL stream + leadership lease (HA only) |

### Gateway (Go)

| Variable                    | Required | Default                    | Description                        |
|-----------------------------|----------|----------------------------|------------------------------------|
| `KNOWLEDGE_API_KEY`         | No       | (empty — auth disabled)    | Static bearer token                |
| `KNOWLEDGE_JWT_SECRET`      | No       | (empty — JWT disabled)     | HMAC secret for tenant JWTs        |
| `KNOWLEDGE_GATEWAY_ADDR`    | No       | `:8080`                    | Public bind address                |
| `KNOWLEDGE_SUBSTRATE_URL`   | No       | `http://127.0.0.1:9090`   | Substrate loopback URL (primary)   |
| `KNOWLEDGE_SUBSTRATE_URL_STANDBY` | No | (empty — single substrate) | Standby substrate URL for active-passive HA; enables write failover + read offload |
| `KNOWLEDGE_DATABASE_URL`    | No       | (empty — in-memory store)  | Postgres connection string         |
| `KNOWLEDGE_NATS_URL`        | No       | (empty — audit disabled)   | NATS JetStream URL                 |
| `KNOWLEDGE_RATE_IP_RPS`     | No       | `50`                       | Per-IP request-per-second limit    |
| `KNOWLEDGE_RATE_TENANT_RPS` | No       | `200`                      | Per-tenant request-per-second limit|
| `KNOWLEDGE_RATE_BURST`      | No       | `100`                      | Token-bucket burst size            |
| `KNOWLEDGE_CORS_ORIGINS`    | No       | `*`                        | Comma-separated allow-list         |
| `KNOWLEDGE_SYNC_INTERVAL`   | No       | `15m`                      | Default connector sync cadence     |
| `KNOWLEDGE_PUBLIC_BASE_URL` | No       | `http://127.0.0.1:8080`   | Externally reachable base URL      |

### Postgres

| Variable            | Default                        |
|---------------------|--------------------------------|
| `POSTGRES_USER`     | `knowledge`                    |
| `POSTGRES_PASSWORD` | **Required** — no default; compose fails to start if unset |
| `POSTGRES_DB`       | `knowledge`                    |

### MinIO

| Variable              | Default                      |
|-----------------------|------------------------------|
| `MINIO_ROOT_USER`     | `minioadmin`                 |
| `MINIO_ROOT_PASSWORD` | **Required** — no default; compose fails to start if unset |
| `MINIO_BUCKET`        | `knowledge`                  |

### Grafana

| Variable            | Default                        |
|---------------------|--------------------------------|
| `GF_ADMIN_USER`     | `admin`                        |
| `GF_ADMIN_PASSWORD` | **Required** — no default; compose fails to start if unset |

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
