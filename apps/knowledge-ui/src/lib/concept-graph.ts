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
  ConceptGraphData,
  ConceptNode,
  MemoryRecord,
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
