# Module Evolution

Per-phase responsibility additions for every Rust shared-core
module. The base per-module summary lives in
[`ARCHITECTURE.md`](../ARCHITECTURE.md) §2.1; this document is the
phase-by-phase evolution catalogue.

## observation_engine

### Phase 2
- Channel-scoped promotion policy with configurable minimum
  importance class, minimum corroboration count, and maximum
  noise ratio.
- Extractor hardening: URL, email, date / time, numeric, and
  question detection.

### Phase 4
- Document observation pipeline: chunking, importance tagging,
  and entity / topic extraction with citation metadata.

## memory_manager

### Phase 2
- Channel Memory Object covering recaps, decisions, open
  questions, and active tasks.
- Archive-on-decay sweep across channel-scoped items.

### Phase 3
- Domain Memory Object covering cross-channel workstreams,
  dependencies, risks, and procedures, with archive-on-decay
  sweep.
- Tenant Memory Object covering canonical policies, product
  taxonomy, stable org knowledge, and approved-document
  references. Tenant items default to a critical sensitivity
  class and do not decay passively; they are removed only by
  explicit deprecation.

## synthesis_pipeline

### Phase 3
- Type-enforced hierarchy module: channel, domain, and tenant
  outputs are distinct types; domain synthesis can only consume
  channel outputs registered on the domain; tenant synthesis can
  only consume domain outputs registered on the tenant plus
  admitted approved documents.
- Scope-tiered synthesis window manager that rejects cross-tier
  admission at the type level.

## concept_graph

### Phase 3
- SQLCipher-backed persistent concept graph layered on top of the
  in-memory adjacency representation.
- Per-scope AEAD encryption of the JSON-encoded payloads, with
  scope and id bound into the AAD.

## permission_service

### Phase 3
- Zanzibar-style relation tuples covering all platform object
  types, a seven-relation namespace with default inheritance
  (`owner ⇒ admin ⇒ editor ⇒ member ⇒ viewer`), an in-memory
  tuple store, and a reachability-style permission check that
  walks both direct tuples and userset rewrites.

## tenant_service

### Phase 3
- Tenant, tenant config, and member data model.
- Lifecycle state machine (`Active / Suspended / Deleted`) that
  destroys per-tenant keys on delete.
- Role-based member provisioning and config validation.

## synthesis_engine

### Phase 3
- Rust side of the server-side synthesis service, including the
  deterministic stub that the future managed-AI endpoint
  replaces.
- End-to-end channel → domain → tenant integration test. The Go
  gateway in front of this engine is intentionally still pending.

## audit_service

### Phase 3
- Append-only audit log with scope, action, actor, and time-range
  filters.
- Eight action types covering canonical promotions, exports,
  agent proposals, policy changes, member provisioning and
  removal, tenant lifecycle, and key destruction. No mutation or
  deletion APIs are exposed.

### Phase 5
- Five additional action types covering export rendering, export
  simulation, and the agent proposal submitted / promoted /
  rejected events.
- Helper functions so every export and every proposal lifecycle
  event produces an audit entry without callers hand-building the
  metadata payload.

## agent_contract

### Phase 5
- Agent write contract with typed proposal payloads for
  observations, concepts, relations, and summaries.
- Strict reuse of the substrate's provenance bundle and
  sensitivity class.
- Schema validation covering confidence range, evidence
  references, scope, agent identity, and TTL.
- Four-state proposal lifecycle (`Proposed → UnderReview →
  Promoted / Rejected`) with an auto-promotion policy that can
  require human review for critical proposals.
- Promotion produces a canonical artifact ready for substrate
  insertion. Agents have only proposer rights and cannot write
  canonical state directly.

## export_plane

### Phase 5
- Portable concept profile with approved concepts and summaries,
  plus the export view variants (concepts only, with summaries,
  with evidence pack).
- Export policy engine that enforces least-privilege defaults:
  raw evidence is opt-in and additionally blocked whenever any
  concept is critical; provenance is required by default;
  sensitivity ceiling, scope whitelist, max concepts, and time
  window are checked per concept.
- Deny-by-default export control registry per concept, summary,
  and workflow with time- and scope-bound enforcement.
- Read-only policy simulator that returns included / excluded
  artifacts and an estimated export size without producing a real
  export.
- Concept approval workflow that bridges canonical concept-graph
  nodes to approved concepts.

## connector_framework

### Phase 4
- Connector trait covering authenticate, initial sync, incremental
  sync, webhook subscription, and webhook event handling.
- OAuth2 token vault with HKDF-derived secret-token wrappers and a
  configurable refresh skew.
- Sync state tracking (full / incremental cursors and last-sync
  time) and sync outcomes.
- Webhook subscription with HMAC-SHA256 signature verification and
  event parsing.
- Connector configuration and runtime instance state, plus
  document and permission event types.
- Channel-scoped attachment with a one-connector-per-source-per-
  scope registry, integrated with the permission service so only
  scope admins or editors can attach or detach a connector.
- ACL sync that idempotently upserts and revokes relation tuples
  in the permission service.
