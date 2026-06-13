// Typed mirrors of the gateway / substrate JSON contracts consumed by
// the end-user UI.
//
// Sources (kept in sync by hand — there is no codegen yet):
//   - Gateway routes:   server/internal/gateway/{gateway,evidence,synthesis}.go
//   - Substrate DTOs:   crates/ffi/src/types.rs
//
// Where the gateway forwards the substrate response verbatim as
// `json.RawMessage`, the shape below reflects the substrate's serde
// output (snake_case fields, PascalCase enum tags).

// ── Enums ───────────────────────────────────────────────────────────

/** `crates/ffi/src/types.rs` SourceKind — the connector a row came from. */
export type SourceKind =
  | 'Manual'
  | 'Slack'
  | 'Email'
  | 'MicrosoftGraph'
  | 'Atlassian'
  | 'HubSpot'
  | 'GoogleWorkspace'
  | 'Other';

/** `crates/ffi/src/types.rs` Importance — retention priority at ingest. */
export type Importance = 'Critical' | 'Important' | 'Useful' | 'Noise';

/**
 * `crates/ffi/src/types.rs` MemoryState — the decay state machine the
 * substrate actually persists. (The conceptual lifecycle in the product
 * docs — candidate → reinforced → consolidated → canonical → superseded
 * → archived → deleted — is a superset; the wire enum is these five.)
 */
export type MemoryState =
  | 'Candidate'
  | 'Reinforced'
  | 'Decaying'
  | 'Archived'
  | 'Pinned';

/** `crates/ffi/src/types.rs` SynthesisTrigger. */
export type SynthesisTrigger =
  | 'ManualUserAction'
  | 'BackgroundIdle'
  | 'EvidenceThreshold'
  | 'ConnectorSyncCompleted';

// ── Evidence / query ────────────────────────────────────────────────

/** Body of `POST /api/v1/ingest`. */
export interface IngestRequest {
  scope_id: string;
  body: string;
  source?: SourceKind;
  importance?: Importance;
}

/** `{ "id": "<uuid>" }` reply from create-style routes. */
export interface IdResponse {
  id: string;
}

/** Body of `POST /api/v1/query`. */
export interface QueryRequest {
  scope_id: string;
  query_text: string;
  limit?: number;
}

/** One hit from `POST /api/v1/query` (substrate QueryResult). */
export interface QueryResult {
  evidence_id: string;
  /** Combined hybrid score in [0, 1]. */
  score: number;
  /** Full-text (FTS5) contribution. */
  fts_score: number;
  /** Recency contribution. */
  recency_score: number;
  /** Semantic-vector contribution. */
  vector_score: number;
  /** Optional snippet (may be empty). */
  snippet: string;
}

/** `GET /api/v1/evidence/{id}` (substrate EvidenceRecord). */
export interface EvidenceRecord {
  id: string;
  scope_id: string;
  body: string;
  source: SourceKind | string;
  /** Unix epoch seconds. */
  created_at: number;
  language_tag?: string | null;
}

// ── Memories ────────────────────────────────────────────────────────

/** A memory state filter accepted by `GET /api/v1/memories`. */
export type MemoryFilter =
  | 'pinned'
  | 'candidate'
  | 'reinforced'
  | 'decaying'
  | 'archived';

/**
 * Body of `POST /api/v1/memories` — write a single user-memory
 * observation for a scope. `sensitivity` is optional; when omitted the
 * substrate applies its default importance class.
 */
export interface CreateMemoryRequest {
  scope_id: string;
  observation_type: string;
  content: string;
  sensitivity?: Importance;
}

/** `GET /api/v1/memories` row (substrate MemoryRecord). */
export interface MemoryRecord {
  id: string;
  scope_id: string;
  summary: string;
  state: MemoryState | string;
  /** Retention score in [0, 1]. */
  retention_score: number;
  /** Unix epoch seconds. */
  created_at: number;
  /** Unix epoch seconds. */
  last_reinforced_at: number;
}

// ── Synthesis ───────────────────────────────────────────────────────

/** Body of `POST /api/v1/synthesis/trigger`. */
export interface SynthesisTriggerRequest {
  scope_id: string;
  trigger?: SynthesisTrigger;
}

/**
 * Synthesis status/recent documents are forwarded from the substrate
 * verbatim and are not yet a stabilised typed contract; the named
 * fields are the ones the UI relies on and the rest are preserved.
 */
export interface SynthesisRecord {
  id?: string;
  scope_id?: string;
  status?: string;
  trigger?: string;
  progress?: number;
  detail?: string;
  created_at?: string | number;
  updated_at?: string | number;
  [key: string]: unknown;
}

// ── Health ──────────────────────────────────────────────────────────

/** Gateway `GET /health` envelope (server/internal/gateway/health.go). */
export interface GatewayHealth {
  status: 'ok' | 'degraded';
  subsystems: Record<string, unknown>;
}

// ── Concept graph (derived, client-side) ────────────────────────────
//
// The gateway does not (yet) expose a typed concept-graph endpoint, so
// the UI derives a best-effort graph from the memory rows of a scope.
// These types describe that derived structure so the ConceptGraph
// component stays decoupled from how the graph is sourced.

export type ConceptEdgeKind = 'relation' | 'supersession' | 'contradiction';

export interface ConceptNode {
  id: string;
  label: string;
  /** Decay state used to colour the node. */
  state: MemoryState | string;
  /** Retention score in [0, 1], used to size the node. */
  weight: number;
}

export interface ConceptEdge {
  source: string;
  target: string;
  kind: ConceptEdgeKind;
  label?: string;
}

export interface ConceptGraphData {
  nodes: ConceptNode[];
  edges: ConceptEdge[];
}

// ── Concept graph (server-derived, GET /api/v1/memories/concept-graph) ──
//
// The substrate projects the per-scope concept graph from live
// user-memory observations and returns this wire-flat `GraphView`
// (crates/concept_graph/src/visualization.rs). These types mirror that
// serde output; `mapGraphView` in lib/concept-graph.ts adapts it to the
// `ConceptGraphData` the ConceptGraph component renders.

/** `concept_graph::NodeState` — coarser than the memory state machine. */
export type GraphNodeState =
  | 'Candidate'
  | 'Canonical'
  | 'Superseded'
  | 'Contradicted'
  | 'Deleted';

/** `concept_graph::RelationType` (snake_case wire tags). */
export type GraphRelationType =
  | 'is_a'
  | 'part_of'
  | 'decided_by'
  | 'supersedes'
  | 'contradicts'
  | 'derived_from'
  | 'assigned_to';

/** A node in the server `GraphView` (substrate NodeVisual). */
export interface GraphNodeVisual {
  id: string;
  label: string;
  state: GraphNodeState;
  scope_id: string;
  position_hint?: { x: number; y: number } | null;
  connections_count: number;
}

/** An edge in the server `GraphView` (substrate EdgeVisual). */
export interface GraphEdgeVisual {
  id: string;
  from: string;
  to: string;
  relation_type: GraphRelationType | string;
  scope_id: string;
}

/** `GET /api/v1/memories/concept-graph` response (substrate GraphView). */
export interface GraphView {
  nodes: GraphNodeVisual[];
  edges: GraphEdgeVisual[];
  scope_filter: string[];
  depth: number;
  truncation: string;
}

// ── Reasoning plane ─────────────────────────────────────────────────

/**
 * One opposing-claim pair surfaced by the substrate's contradiction
 * detector (FFI `ContradictionView`). The "what contradicts" surface.
 */
export interface ContradictionView {
  id: string;
  left_id: string;
  left_label: string;
  right_id: string;
  right_label: string;
  /** Detector confidence in `[0, 1]`. */
  confidence: number;
  left_evidence_count: number;
  right_evidence_count: number;
  detected_at: string;
}

/** Why a canonical claim's evidence base shifted (FFI `DriftReason`). */
export type DriftReason =
  | 'evidence_superseded'
  | 'evidence_removed'
  | 'evidence_weakened';

/**
 * A canonical claim whose supporting evidence base has shifted (FFI
 * `DriftView`). The "what changed" surface.
 */
export interface DriftView {
  node_id: string;
  label: string;
  reason: DriftReason;
  evidence_at_promotion: number;
  evidence_remaining: number;
  detected_at: string;
}

/** One step in an explained query plan (FFI `ExplainStepView`). */
export interface ExplainStepView {
  /** Retrieval mode (snake_case wire tag). */
  mode: string;
  /** Cost rank — lower is cheaper. */
  cost_rank: number;
  time_budget_ms?: number | null;
}

/**
 * The query planner's rationale for a retrieval (FFI
 * `QueryExplanationView`). The "why this answer" surface.
 */
export interface QueryExplanationView {
  query: string;
  /** Query class (snake_case wire tag). */
  class: string;
  steps: ExplainStepView[];
  rationale: string;
  planned_at: string;
}

/** `POST /api/v1/reasoning/explain` request body. */
export interface ExplainQueryRequest {
  scope_id: string;
  query: string;
}
