// Derive a best-effort concept graph from a scope's memory rows.
//
// The gateway does not (yet) expose a typed concept-graph endpoint, so
// the UI builds an explainable, deterministic graph client-side:
//   - Each memory becomes a node (coloured by decay state, sized by
//     retention score).
//   - Edges are lexical-overlap relations between memory summaries
//     (Jaccard similarity over salient tokens, above a threshold).
//   - An overlapping pair where one side is Archived and the other is
//     Reinforced/Pinned is rendered as a `supersession` edge (the live
//     memory supersedes the archived one).
//
// This is intentionally a heuristic and is labelled as "derived" in the
// UI; it is a drop-in seam for a real graph endpoint later — the
// ConceptGraph component only depends on the `ConceptGraphData` shape.

import type {
  ConceptEdge,
  ConceptEdgeKind,
  ConceptGraphData,
  ConceptNode,
  GraphNodeVisual,
  GraphView,
  MemoryRecord,
  MemoryState,
} from './types';

const STOPWORDS = new Set([
  'the', 'a', 'an', 'and', 'or', 'but', 'of', 'to', 'in', 'on', 'for', 'with',
  'is', 'are', 'was', 'were', 'be', 'been', 'this', 'that', 'these', 'those',
  'it', 'its', 'as', 'at', 'by', 'from', 'has', 'have', 'had', 'will', 'would',
  'about', 'into', 'over', 'than', 'then', 'they', 'them', 'their', 'you',
]);

function tokenize(text: string): Set<string> {
  return new Set(
    text
      .toLowerCase()
      .split(/[^a-z0-9]+/)
      .filter((w) => w.length > 2 && !STOPWORDS.has(w)),
  );
}

function jaccard(a: Set<string>, b: Set<string>): number {
  if (a.size === 0 || b.size === 0) return 0;
  let inter = 0;
  for (const t of a) if (b.has(t)) inter++;
  const union = a.size + b.size - inter;
  return union === 0 ? 0 : inter / union;
}

function truncate(text: string, max = 48): string {
  const t = text.trim();
  return t.length <= max ? t : `${t.slice(0, max - 1)}…`;
}

const ARCHIVED = new Set(['archived', 'decaying']);
const LIVE = new Set(['reinforced', 'pinned']);

/** Build a derived concept graph from memory rows. */
export function buildConceptGraph(
  memories: MemoryRecord[],
  threshold = 0.18,
): ConceptGraphData {
  const nodes: ConceptNode[] = memories.map((m) => ({
    id: m.id,
    label: truncate(m.summary || m.id),
    state: m.state,
    weight: typeof m.retention_score === 'number' ? m.retention_score : 0,
  }));

  const tokens = memories.map((m) => tokenize(m.summary || ''));
  const edges: ConceptEdge[] = [];

  for (let i = 0; i < memories.length; i++) {
    for (let j = i + 1; j < memories.length; j++) {
      const sim = jaccard(tokens[i], tokens[j]);
      if (sim < threshold) continue;
      const a = String(memories[i].state).toLowerCase();
      const b = String(memories[j].state).toLowerCase();
      const supersession =
        (ARCHIVED.has(a) && LIVE.has(b)) || (ARCHIVED.has(b) && LIVE.has(a));
      // Orient supersession edges live → archived.
      const [src, tgt] = supersession && ARCHIVED.has(a)
        ? [memories[j].id, memories[i].id]
        : [memories[i].id, memories[j].id];
      edges.push({
        source: src,
        target: tgt,
        kind: supersession ? 'supersession' : 'relation',
        label: `${Math.round(sim * 100)}%`,
      });
    }
  }

  return { nodes, edges };
}

// The component colours nodes by the UI memory-state vocabulary
// (`MemoryState`: Candidate/Reinforced/Decaying/Archived/Pinned). The
// server graph speaks the coarser concept-graph NodeState vocabulary, so
// map each node state onto the closest `MemoryState` bucket: a live
// `Candidate` stays "Candidate", a `Canonical` concept reads as the live
// "Reinforced" colour, and `Superseded`/`Contradicted` (decayed-out or
// conflicting) as "Archived". Values are the PascalCase `MemoryState`
// vocabulary — identical to what `buildConceptGraph` emits from
// `m.state` — so both graph paths feed the same tooltip text and colour
// lookup (`ConceptGraph` lowercases before `STATE_COLORS`).
const NODE_STATE_CLASS: Record<string, MemoryState> = {
  Candidate: 'Candidate',
  Canonical: 'Reinforced',
  Superseded: 'Archived',
  Contradicted: 'Archived',
  Deleted: 'Archived',
};

function edgeKindFor(relation: string): ConceptEdgeKind {
  if (relation === 'supersedes') return 'supersession';
  if (relation === 'contradicts') return 'contradiction';
  return 'relation';
}

/**
 * Adapt the substrate's wire-flat {@link GraphView} into the
 * {@link ConceptGraphData} the ConceptGraph component renders.
 *
 * The server graph is the source of truth (projected from live
 * user-memory); this only reshapes field names and vocabularies. Node
 * size (`weight`) is taken from the matching memory's retention score
 * when available — the two share the same id — so the graph and the
 * memory list stay visually consistent; otherwise it falls back to a
 * connection-count heuristic so well-connected concepts still read as
 * more prominent. Edges whose endpoints were truncated server-side are
 * dropped so the layout never references a missing node.
 */
export function mapGraphView(
  view: GraphView,
  retentionById?: Map<string, number>,
): ConceptGraphData {
  // The wire contract declares `nodes`/`edges` as arrays, but coalesce a
  // missing or `null` value to `[]` so a malformed/legacy payload renders
  // an honest empty graph instead of throwing on `.reduce`/`.map` — the
  // same defensive posture `listMemories` takes with `asArray`.
  const viewNodes = view.nodes ?? [];
  const viewEdges = view.edges ?? [];
  // Treat a non-finite `connections_count` (a contract violation) as 0
  // rather than letting one bad node poison `maxConnections` with `NaN`,
  // which would collapse every retention-less node to weight 0.
  const conn = (n: GraphNodeVisual): number =>
    Number.isFinite(n.connections_count) ? n.connections_count : 0;
  const maxConnections = viewNodes.reduce((m, n) => Math.max(m, conn(n)), 0);
  const nodes: ConceptNode[] = viewNodes.map((n) => {
    const retention = retentionById?.get(n.id);
    const weight =
      typeof retention === 'number'
        ? retention
        : maxConnections > 0
          ? conn(n) / maxConnections
          : 0;
    return {
      id: n.id,
      label: n.label,
      state: NODE_STATE_CLASS[n.state] ?? String(n.state),
      weight,
    };
  });

  const present = new Set(nodes.map((n) => n.id));
  const edges: ConceptEdge[] = [];
  for (const e of viewEdges) {
    if (!present.has(e.from) || !present.has(e.to)) continue;
    edges.push({
      source: e.from,
      target: e.to,
      kind: edgeKindFor(String(e.relation_type)),
    });
  }

  return { nodes, edges };
}
