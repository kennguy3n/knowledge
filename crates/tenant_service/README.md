# tenant_service

Tenant lifecycle and member-provisioning data model for the Knowledge
substrate.

## Purpose

Owns tenant lifecycle (Active -> Suspended -> Deleted), per-tenant
configuration (encryption key references, storage config, synthesis
config), and member provisioning (user-to-tenant membership with
permission roles).

## Public API summary

| Type / Function | Description |
|---|---|
| `Tenant` / `TenantId` | Tenant entity and identifier. |
| `TenantRegistry` | In-memory tenant map. |
| `PersistentTenantRegistry` | SQLCipher-backed persistent registry. |
| `TenantConfig` / `TenantKeyRef` | Per-tenant configuration. |
| `TenantMember` / `TenantMemberStatus` | Member provisioning. |
| `TenantStatus` | Lifecycle state enum. |

## Links

- [ARCHITECTURE.md](../../ARCHITECTURE.md) §4.1 — Tenant service.
- [docs/INTEGRATION_GUIDE.md](../../docs/INTEGRATION_GUIDE.md) — Consumer integration guide.
