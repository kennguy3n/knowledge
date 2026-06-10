# Upgrading

This guide covers how Knowledge is versioned, how to move between
versions safely, what counts as a breaking change, and how to roll
back. For the initial install and topology see the
[deployment guide](deployment-guide.md); for backups (a prerequisite
for any upgrade) see [backup & recovery](backup-recovery.md).

## Version policy

Knowledge is a single-version Cargo workspace: every crate inherits
`version.workspace = true` from the root `Cargo.toml`, so the whole
substrate ships under one SemVer number. The Go gateway, the Docker
images, and any packaged Helm chart are released under that **same**
version, and the substrate stamps it into `CARGO_PKG_VERSION` (surfaced
on `/health` and, opt-in, on `/internal/update_check`).

We follow [Semantic Versioning 2.0.0](https://semver.org/):

| Component   | Bumped when …                                                            |
| ----------- | ------------------------------------------------------------------------ |
| **MAJOR**   | A backward-incompatible change ships (see [breaking changes](#breaking-change-policy)). |
| **MINOR**   | Backward-compatible functionality is added (new endpoint, new connector, new config with a safe default). |
| **PATCH**   | Backward-compatible bug fixes and security patches only.                 |

Pre-1.0 (`0.y.z`) releases treat the **minor** as the breaking lane:
`0.y` → `0.(y+1)` may carry breaking changes, while `0.y.z` → `0.y.(z+1)`
stays compatible. The `CHANGELOG.md` is the source of truth for what
changed in each release and is grouped under `Added` / `Changed` /
`Fixed` / `Security` / `Breaking`.

> **Note (version drift).** `CHANGELOG.md` documents a `1.0.0` public
> release, while the workspace `Cargo.toml` is still at `0.1.0`. Cutting
> the first tagged release is a deliberate step: set the workspace
> `version` to match the intended tag (e.g. `1.0.0`), update
> `crates/napi/package.json` to the same value, land that as its own
> "release vX.Y.Z" commit, then push the `vX.Y.Z` tag. The release
> automation keys off the tag, not the manifest, so the two must agree
> before tagging.

### Minimum Supported Rust Version (MSRV)

The workspace MSRV is **1.88.0**, pinned via `rust-version.workspace`.
Raising the MSRV is itself a breaking change for anyone building from
source and is called out in the changelog.

## How releases are produced

Pushing a `v*` tag triggers `.github/workflows/release.yml`, which:

1. Re-runs the full `cargo test --all-features --workspace` matrix on
   the tagged commit.
2. Builds stripped `substrate_server` release binaries for
   `x86_64`/`aarch64` Linux and attaches them (with `.sha256`
   checksums) to the GitHub Release.
3. Builds and pushes Docker images to GHCR:
   - `ghcr.io/<owner>/knowledge/substrate`
   - `ghcr.io/<owner>/knowledge/gateway`
   - `ghcr.io/<owner>/knowledge/llama-server`

   each tagged `:vX.Y.Z`, `:X.Y`, and `:latest`.
4. Packages the Helm chart **if one exists** in the tree (the chart is
   owned by the deployment workstream); otherwise this step is a no-op.

A separate scheduled workflow,
`.github/workflows/auto-update-check.yml`, opens/refreshes a weekly
issue summarising `cargo update --dry-run`, `cargo outdated`, and
`cargo audit` so dependency drift and advisories stay visible between
releases.

## Upgrade procedure

> **Always take a backup first.** Run a
> [backup & recovery](backup-recovery.md) snapshot of the substrate
> data volume and the Postgres database before upgrading. The substrate
> store is SQLCipher-encrypted — back up the `*.db` file **and** keep
> `KNOWLEDGE_MASTER_KEY` safe; the data is unrecoverable without it.

### Docker Compose (pre-built images)

`deploy/docker-compose.yml` declares both `build:` and `image:` for the
first-party services, so you can run released images instead of
building from source:

```bash
# Pin the release you want and pull it.
export KNOWLEDGE_IMAGE_TAG=v1.0.0
docker compose -f deploy/docker-compose.yml pull
docker compose -f deploy/docker-compose.yml up -d
```

> **Requires Docker Compose v2.20.0+.** The `image:` entries use nested
> variable defaults (`${…:-…:${KNOWLEDGE_IMAGE_TAG:-latest}}`), which
> only resolve on Compose v2.20.0 or newer (Docker Engine 24.0.6+). On
> older Compose the inner `${…}` is treated literally and `pull` fails
> with a malformed image reference — check with `docker compose version`.

Leaving `KNOWLEDGE_IMAGE_TAG` unset (or running `up` without a prior
`pull`) falls back to the `:latest` tag / a local source build, so
existing build-from-source workflows are unchanged. Individual image
references can be overridden with `KNOWLEDGE_SUBSTRATE_IMAGE`,
`KNOWLEDGE_GATEWAY_IMAGE`, and `KNOWLEDGE_LLAMA_IMAGE`.

Recommended sequence for a minor/patch upgrade:

1. Read the `CHANGELOG.md` entry for the target version; note any
   `Breaking` items and required config/env changes.
2. Back up the substrate volume + Postgres.
3. Bump `KNOWLEDGE_IMAGE_TAG`, `docker compose pull`, then
   `docker compose up -d`. Compose recreates only changed services.
4. Watch health: the substrate exposes `/health` and
   `/internal/metrics` on `:9090`; the gateway has a `/healthcheck`
   binary wired into its container healthcheck. Wait for all services
   to report healthy.
5. Smoke-test a representative ingest + query before re-enabling
   traffic.

### From source

```bash
git fetch --tags
git checkout v1.0.0
docker compose -f deploy/docker-compose.yml up --build -d
```

### Checking for updates

The substrate ships an **opt-in** update check that compares the
running `CARGO_PKG_VERSION` against the latest GitHub Release tag. It is
**disabled by default** and never runs at startup. Enable it by setting:

```bash
KNOWLEDGE_UPDATE_CHECK_ENABLED=1
# Optional overrides (defaults shown):
# KNOWLEDGE_UPDATE_CHECK_REPO=kennguy3n/knowledge
# KNOWLEDGE_UPDATE_CHECK_API_BASE=https://api.github.com
```

Then query it on demand:

```bash
curl -s http://localhost:9090/internal/update_check | jq
# { "enabled": true, "current_version": "1.0.0",
#   "latest_version": "1.1.0", "update_available": true }
```

When disabled, the endpoint returns `{"enabled": false, …}` without any
network access. The lookup uses the substrate's standard injected HTTP
transport and only performs network I/O in builds compiled with the
`http-client` feature (production deployments); otherwise it reports the
subsystem as unavailable. It is purely advisory — it never downloads or
applies anything.

## Breaking-change policy

A change is **breaking** (and therefore MAJOR, or pre-1.0 MINOR) if it
requires an operator or API consumer to change something to keep
working. This includes:

- Removing or renaming a REST endpoint, request/response field, or
  changing a field's type or semantics.
- Removing or renaming a configuration environment variable, or
  changing its default in a way that alters behaviour.
- An on-disk schema migration that is not transparently applied on
  startup, or that cannot be rolled back.
- Raising the MSRV (for source builds).
- Removing or renaming a Docker image or a published artifact.

Breaking changes are:

1. Called out under a `Breaking` heading in `CHANGELOG.md`.
2. Accompanied by a migration note describing the required operator
   action.
3. Where feasible, preceded by a deprecation period in a prior
   minor release (the old behaviour keeps working and logs a warning).

Backward-compatible additions — new endpoints, new optional config with
safe defaults (the `update_check` endpoint above is an example) — are
**not** breaking and ship in a minor release.

## Rollback

Because images and artifacts are immutable per tag, rollback is a
redeploy of the previous version — provided the data schema is
compatible.

1. **Re-pin the previous tag** and redeploy:

   ```bash
   export KNOWLEDGE_IMAGE_TAG=v0.9.0   # the last-known-good release
   docker compose -f deploy/docker-compose.yml pull
   docker compose -f deploy/docker-compose.yml up -d
   ```

2. **If the upgrade ran a one-way schema migration**, a redeploy of the
   old binary against the new data may fail to open the store. In that
   case restore the pre-upgrade backup of the substrate volume +
   Postgres taken in step 2 of the upgrade procedure, then redeploy the
   old tag against the restored data. This is why a backup is mandatory
   before every upgrade, and why migrations that cannot be rolled back
   are flagged as breaking.

3. **Verify** health (`/health`, `/internal/metrics`) and run the same
   ingest + query smoke test before re-admitting traffic.

4. **Capture** what went wrong against the
   [incident runbook](runbook.md) so the failed upgrade can be
   reproduced and fixed before the next attempt.

### Rollback decision table

| Situation                                              | Action                                                            |
| ------------------------------------------------------ | ----------------------------------------------------------------- |
| New version misbehaves, **no** schema change           | Re-pin previous tag and redeploy. No data restore needed.         |
| New version misbehaves, **backward-compatible** migration | Re-pin previous tag; data stays as-is.                         |
| New version misbehaves, **one-way** migration          | Restore pre-upgrade backup, then redeploy previous tag.           |
| Corrupted/again-unhealthy after restore                | Escalate via the [runbook](runbook.md); do not re-admit traffic.  |
