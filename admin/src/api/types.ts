// Typed mirrors of the gateway / substrate JSON contracts.
//
// Sources (kept in sync by hand — there is no codegen yet):
//   - Gateway routes:        server/internal/gateway/gateway.go
//   - Health envelope:       server/internal/gateway/health.go
//                            crates/ffi/src/health.rs (HealthStatus)
//   - Connectors:            server/internal/connector/service.go
//                            crates/ffi/src/types.rs (ConnectorStatus)
//   - Tenants:               server/internal/tenant/{tenant,model}.go
//   - Audit:                 server/internal/audit/{service,event}.go
//
// Where the gateway passes the substrate response through verbatim as
// `json.RawMessage`, the shape below reflects the substrate's serde
// output (camelCase fields, snake_case enum tags). Fields the admin
// does not yet render are typed but optional.

// ── Health ──────────────────────────────────────────────────────────

export type SubsystemStatus = 'ok' | 'degraded' | 'unavailable';

export interface SubsystemHealth {
  name: string;
  status: SubsystemStatus;
  detail?: string | null;
}

/** Substrate `GET /health` detail (crates/ffi/src/health.rs HealthStatus). */
export interface SubstrateHealth {
  core_version: string;
  uptime_secs: number;
  tracing_initialized: boolean;
  subsystems: SubsystemHealth[];
  // `metrics` is a large MetricsSnapshot; surfaced opaquely for now.
  metrics?: Record<string, unknown>;
}

/**
 * Gateway `GET /health` envelope (server/internal/gateway/health.go).
 * `subsystems` maps a subsystem name to either a plain status string
 * (e.g. "ok" / "disabled" / "down") or, for the special
 * `substrate_detail` key, the inlined substrate health document.
 */
export interface GatewayHealth {
  status: 'ok' | 'degraded';
  subsystems: Record<string, string | SubstrateHealth>;
}

// ── Connectors ──────────────────────────────────────────────────────

export type ConnectorKind =
  | 'google_drive'
  | 'one_drive'
  | 'notion'
  | 'jira'
  | 'confluence'
  | 'git_hub'
  | 'slack'
  | 'figma'
  | 'hub_spot'
  | 'email'
  | 'generic_webhook';

export type SyncMode = 'full' | 'incremental';
export type SyncStatus = 'never_run' | 'in_progress' | 'succeeded' | 'failed';

/** Substrate `GET /connectors` row (crates/ffi/src/types.rs ConnectorStatus). */
export interface ConnectorStatus {
  instanceId: string;
  kind: ConnectorKind | string;
  scopeId: string;
  syncMode: SyncMode | string;
  syncStatus: SyncStatus | string;
  lastSyncedAt?: number | null;
  lastError?: string | null;
}

/** Body for `POST /api/v1/connectors`. */
export interface CreateConnectorRequest {
  kind: string;
  scope_id: string;
  config_json?: string;
}

/** Body for `POST /api/v1/connectors/{id}/authenticate`. */
export interface AuthenticateConnectorRequest {
  auth_code: string;
}

// ── Tenants ─────────────────────────────────────────────────────────

export type SynthesisTier = 'basic' | 'standard' | 'premium';

export interface TenantConfig {
  connector_limit: number;
  synthesis_tier: SynthesisTier;
  retention_days: number;
}

export interface CryptoKey {
  algorithm: string;
  public_key_hex: string;
}

export interface Tenant {
  id: string;
  name: string;
  config: TenantConfig;
  key: CryptoKey;
  created_at: string;
}

export interface CreateTenantRequest {
  name: string;
  config?: TenantConfig;
}

export type MemberStatus = 'invited' | 'active' | 'suspended';

export interface Member {
  tenant_id: string;
  user_id: string;
  email: string;
  status: MemberStatus;
  updated_at: string;
}

// ── Synthesis ───────────────────────────────────────────────────────

export interface SynthesisTriggerRequest {
  scope_id: string;
  trigger?: string;
}

/**
 * Substrate synthesis status/recent documents are passed through the
 * gateway verbatim and are not yet stabilised into a typed contract.
 * The admin renders them defensively; see api/synthesis.ts.
 *
 * TODO(workstream-7): replace `SynthesisRecord` with the concrete
 * substrate DTO once crates/substrate_server exposes a documented
 * synthesis status schema (it currently returns an opaque object).
 */
export interface SynthesisRecord {
  id?: string;
  scope_id?: string;
  status?: string;
  trigger?: string;
  created_at?: string;
  [key: string]: unknown;
}

// ── Memories ────────────────────────────────────────────────────────

/**
 * Memory rows are passed through from the substrate `list_memories`
 * FFI call. The substrate does not yet expose a stable typed schema,
 * so only the fields the admin relies on are named; the rest are
 * preserved for display.
 *
 * TODO(workstream-7): pin this to the substrate memory DTO once it is
 * documented.
 */
export interface MemoryRecord {
  id?: string;
  scope_id?: string;
  state?: string;
  pinned?: boolean;
  summary?: string;
  [key: string]: unknown;
}

export type MemoryFilter = 'pinned' | 'candidate' | 'reinforced' | 'archived';

// ── Audit ───────────────────────────────────────────────────────────

export interface AuditEvent {
  id: string;
  tenant_id: string;
  scope_id: string;
  action: string;
  actor: string;
  detail?: unknown;
  created_at: string;
}

export interface AuditQuery {
  tenant_id?: string;
  scope_id?: string;
  action?: string;
  actor?: string;
  from?: string;
  to?: string;
  limit?: number;
}
