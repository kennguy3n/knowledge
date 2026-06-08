# Backup & Recovery

Backup strategies and disaster recovery for a Knowledge deployment.
There are three stateful stores: Postgres (relational), MinIO (objects),
and the SQLCipher substrate database (encrypted user content).

## What to back up

| Store | Contains | Encrypted at rest |
|---|---|---|
| Postgres | Tenants, permissions, connector config, audit metadata | No (back up securely) |
| MinIO | Object payloads | Depends on MinIO config |
| SQLCipher substrate | Evidence bodies, concepts, synthesis | **Yes** — needs the master key to open |

## Postgres

```bash
# Full dump
docker exec <pg_container> pg_dump -U knowledge knowledge > backup.sql

# Restore
docker exec -i <pg_container> psql -U knowledge knowledge < backup.sql
```

Schema migrations are forward-only and additive, so a newer binary can
read an older dump after it re-runs migrations on startup.

## MinIO

```bash
mc alias set local http://localhost:9000 minioadmin minioadmin
mc mirror local/knowledge ./minio-backup/
```

## SQLCipher substrate

The substrate database is a single SQLCipher file on the
`substrate-data` volume:

```bash
docker cp <substrate_container>:/data/substrate.db ./substrate-backup.db
```

The backup is **encrypted at rest**; restoring it requires the same
`KNOWLEDGE_MASTER_KEY`. A backup without the key is unrecoverable —
which is the point, but it means your key-escrow strategy is part of
your backup strategy.

## Key escrow

Because the master key gates the substrate, treat it as the most
critical backup artifact:

- Store the master key in a dedicated secret manager (not next to the
  database backup).
- For enterprise deployments, escrow the key with split knowledge
  (e.g. Shamir shares) so no single operator can unilaterally read user
  data or lose access.
- See [key-management.md](../security/key-management.md) for storage
  and rotation, and note that **rotating** the master key requires
  re-encrypting the substrate (export → new store with new key →
  re-import).

## Disaster recovery drill

1. Provision a fresh stack (`make up` on clean volumes).
2. Restore Postgres and MinIO from backup.
3. Restore the SQLCipher file and provide the escrowed master key.
4. Verify with the [health endpoint](monitoring.md#health-endpoint) and
   a read-back query.

Test this end-to-end on a schedule — an untested backup is a guess.

## Further reading

- [key-management.md](../security/key-management.md) — master-key
  storage and rotation.
- [deployment-guide.md](deployment-guide.md) — stack topology.
- [runbook.md](runbook.md) — incident response.
