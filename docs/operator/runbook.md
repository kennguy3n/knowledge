# Incident Runbook

Response procedures for the alerts defined in
`deploy/prometheus/alerts.yml`. Each entry: what fired, how to confirm,
and how to remediate. Pair this with [monitoring.md](monitoring.md)
(where alerts are defined) and [troubleshooting.md](troubleshooting.md)
(root-cause diagnostics).

## KnowledgeSubsystemDown (critical)

A subsystem health gauge dropped to 0 for 2 minutes.

1. **Confirm:** `curl -s http://localhost:8080/health | jq .` — find
   the subsystem reporting down.
2. **Diagnose:** read that subsystem's logs
   (`docker compose ... logs <service>`).
3. **Remediate:** restart the affected service; if it's the substrate,
   verify `KNOWLEDGE_MASTER_KEY` and store-path permissions (see
   [troubleshooting.md](troubleshooting.md#gateway-returns-502--connection-refused)).

## KnowledgeHighErrorRate (warning)

> 5% error rate for 5 minutes.

1. **Confirm:** Grafana → Error Breakdown by Kind; identify the
   dominant `kind` label.
2. **Diagnose:** correlate with a recent deploy or an upstream
   provider outage.
3. **Remediate:** roll back the last deploy if it correlates; otherwise
   address the specific error class.

## KnowledgeHighLatency (warning)

p99 latency > 500 ms for 5 minutes.

1. **Confirm:** Grafana → Query Rate + Latency panel.
2. **Diagnose:** check substrate CPU/RAM (vertical limit) and Postgres
   pool saturation.
3. **Remediate:** scale per [scaling.md](scaling.md) — vertical for the
   substrate, more replicas for the gateway, Postgres tuning.

## KnowledgeSynthesisBacklog (warning)

Pending syntheses stuck for 30 minutes.

1. **Confirm:** Grafana → Synthesis Trigger/Success; check the throttle
   rate.
2. **Diagnose:** inference backend down/saturated, or rate limits too
   aggressive.
3. **Remediate:** verify `llama-server` health; adjust synthesis rate
   limits in [configuration.md](configuration.md).

## KnowledgeConnectorFailing (warning)

A provider failed 3+ times consecutively.

1. **Confirm:**
   `curl -s -H "Authorization: Bearer $KNOWLEDGE_API_KEY" http://localhost:8080/api/v1/connectors | jq '.[] | {id, kind, status}'`
2. **Diagnose:** expired OAuth tokens, upstream rate limiting, or
   network. See
   [connector-protocol.md](../technical/connector-protocol.md).
3. **Remediate:** re-authorize the connector or back off the sync
   cadence (`KNOWLEDGE_SYNC_INTERVAL`).

## KnowledgeDiskPressure (warning)

Volume usage > 80% for 5 minutes.

1. **Confirm:** check the affected volume.
2. **Diagnose:** which store is growing — Postgres, MinIO, or the
   substrate file.
3. **Remediate:** expand the volume; review retention; ensure backups
   are rotating off-box (see
   [backup-recovery.md](backup-recovery.md)).

## Further reading

- [monitoring.md](monitoring.md) — alert definitions and dashboards.
- [troubleshooting.md](troubleshooting.md) — root-cause diagnostics.
- [backup-recovery.md](backup-recovery.md) — recovery procedures.
