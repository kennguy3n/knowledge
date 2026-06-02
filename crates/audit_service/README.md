# audit_service

Append-only audit log for the Knowledge substrate.

## Purpose

Records an append-only audit log of canonical promotions, exports,
agent proposals, policy changes, and tenant-lifecycle events
(provisioning, deletion, key destruction). Once inserted, entries
cannot be modified or removed.

## Public API summary

| Type / Function | Description |
|---|---|
| `AuditLog` | In-memory append-only log with query support. |
| `PersistentAuditLog` | SQLCipher-backed persistent wrapper. |
| `AuditEntry` / `AuditEntryBuilder` | Individual audit record. |
| `AuditQuery` | Query filter for log retrieval. |
| `log_export`, `log_proposal_promoted`, … | Convenience helpers. |

## Usage example

```rust
use audit_service::{AuditLog, AuditEntryBuilder, AuditActionType, Actor};

let mut log = AuditLog::new();
let entry = AuditEntryBuilder::new(AuditActionType::ExportRendered)
    .actor(Actor::User(user_id))
    .build();
log.append(entry);
```

## Links

- [ARCHITECTURE.md](../../ARCHITECTURE.md) §4.1 — Audit service.
- [docs/INTEGRATION_GUIDE.md](../../docs/INTEGRATION_GUIDE.md) — Consumer integration guide.
