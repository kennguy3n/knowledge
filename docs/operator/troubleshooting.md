# Troubleshooting

Common issues, their causes, and the diagnostic commands to confirm
them. For alert-driven incident response, see the [runbook](runbook.md).

## Gateway returns 502 / connection refused

The substrate_server is not ready yet. Check its logs:

```bash
docker compose -f deploy/docker-compose.yml logs knowledge-substrate
```

Common causes: missing `KNOWLEDGE_MASTER_KEY`, bad store-path
permissions.

## Substrate fails to start with "bad master key"

`KNOWLEDGE_MASTER_KEY` must be exactly 64 hex characters. Generate a
fresh one:

```bash
openssl rand -hex 32
```

See [key-management.md](../security/key-management.md) for how the key
is provisioned and stored.

## Postgres migrations fail

The gateway auto-migrates on startup. If it fails:

1. Check Postgres is reachable: `docker exec -it <pg_container> psql -U knowledge`
2. Check the pgvector extension: `SELECT * FROM pg_extension WHERE extname = 'vector';`

## NATS JetStream not available

Verify JetStream is enabled via the monitoring endpoint:

```bash
curl http://localhost:8222/jsz
```

## High memory usage

- `llama-server` loads the full GGUF model into RAM. Reduce
  `--ctx-size` or use a smaller quantisation.
- The substrate's SQLCipher cache defaults are tuned for moderate
  workloads; adjust `PRAGMA cache_size` via the config if needed.

## Connectors not syncing

Check connector status:

```bash
curl -s -H "Authorization: Bearer $KNOWLEDGE_API_KEY" \
  http://localhost:8080/api/v1/connectors | jq '.[] | {id, kind, status}'
```

Common causes: expired OAuth tokens, upstream rate limiting, network
issues. See [connector-protocol.md](../technical/connector-protocol.md)
for the sync state machine.

## Further reading

- [runbook.md](runbook.md) — alert response procedures.
- [monitoring.md](monitoring.md) — metrics and health probes.
- [configuration.md](configuration.md) — every tunable and its default.
