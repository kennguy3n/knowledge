'use client';

import { useMemo, useState } from 'react';
import type { ConceptEdgeKind, ConceptGraphData } from '@/lib/types';

// Self-contained SVG force-directed graph. A dependency-free renderer is
// used deliberately: vis-network / d3-force pull in large bundles and
// touch `window`/`canvas` at import time, which is awkward under Next's
// static export. The layout is a deterministic seeded circular placement
// refined by a few fixed iterations of repulsion + edge springs, so the
// same data always renders the same graph.

const WIDTH = 720;
const HEIGHT = 460;
// Force layout cost is O(iterations · n²). Keep many iterations for the
// small graphs where convergence matters, but scale them down for large
// node counts so a full memory page (up to ~200 rows) can't tie up the
// main thread. The budget caps total pairwise work at a fixed ceiling.
const MAX_ITERATIONS = 220;
const MIN_ITERATIONS = 40;
const ITERATION_BUDGET = 2_000_000;

function iterationsFor(n: number): number {
  return Math.max(
    MIN_ITERATIONS,
    Math.min(MAX_ITERATIONS, Math.round(ITERATION_BUDGET / (n * n))),
  );
}

interface Positioned {
  id: string;
  label: string;
  state: string;
  weight: number;
  x: number;
  y: number;
}

const STATE_COLORS: Record<string, string> = {
  candidate: '#d29922',
  reinforced: '#3fb950',
  decaying: '#db6d28',
  archived: '#8b5cf6',
  pinned: '#4c8dff',
};

const EDGE_COLORS: Record<ConceptEdgeKind, string> = {
  relation: '#4c8dff',
  supersession: '#3fb950',
  contradiction: '#f85149',
};

function nodeColor(state: string): string {
  return STATE_COLORS[state.toLowerCase()] ?? '#8b98a5';
}

function radius(weight: number): number {
  return 8 + Math.max(0, Math.min(1, weight)) * 14;
}

function layout(data: ConceptGraphData): Positioned[] {
  const n = data.nodes.length;
  if (n === 0) return [];
  const cx = WIDTH / 2;
  const cy = HEIGHT / 2;
  const ring = Math.min(WIDTH, HEIGHT) / 2 - 50;

  const pts: Positioned[] = data.nodes.map((node, i) => {
    const angle = (2 * Math.PI * i) / n;
    return {
      ...node,
      state: String(node.state),
      x: cx + ring * Math.cos(angle),
      y: cy + ring * Math.sin(angle),
    };
  });

  if (n === 1) {
    pts[0].x = cx;
    pts[0].y = cy;
    return pts;
  }

  const index = new Map(pts.map((p, i) => [p.id, i]));
  const k = Math.sqrt((WIDTH * HEIGHT) / n); // ideal spacing
  const iterations = iterationsFor(n);

  for (let iter = 0; iter < iterations; iter++) {
    const dispX = new Array(n).fill(0);
    const dispY = new Array(n).fill(0);

    // Repulsion between every pair.
    for (let i = 0; i < n; i++) {
      for (let j = i + 1; j < n; j++) {
        let dx = pts[i].x - pts[j].x;
        let dy = pts[i].y - pts[j].y;
        let dist = Math.hypot(dx, dy);
        if (dist < 0.01) {
          dx = (i - j) || 0.5;
          dy = 0.5;
          dist = Math.hypot(dx, dy);
        }
        const force = (k * k) / dist;
        const fx = (dx / dist) * force;
        const fy = (dy / dist) * force;
        dispX[i] += fx;
        dispY[i] += fy;
        dispX[j] -= fx;
        dispY[j] -= fy;
      }
    }

    // Attraction along edges.
    for (const e of data.edges) {
      const a = index.get(e.source);
      const b = index.get(e.target);
      if (a === undefined || b === undefined) continue;
      const dx = pts[a].x - pts[b].x;
      const dy = pts[a].y - pts[b].y;
      const dist = Math.hypot(dx, dy) || 0.01;
      const force = (dist * dist) / k;
      const fx = (dx / dist) * force;
      const fy = (dy / dist) * force;
      dispX[a] -= fx;
      dispY[a] -= fy;
      dispX[b] += fx;
      dispY[b] += fy;
    }

    const temp = (1 - iter / iterations) * (k / 2);
    for (let i = 0; i < n; i++) {
      const d = Math.hypot(dispX[i], dispY[i]) || 0.01;
      pts[i].x += (dispX[i] / d) * Math.min(d, temp);
      pts[i].y += (dispY[i] / d) * Math.min(d, temp);
      pts[i].x = Math.max(30, Math.min(WIDTH - 30, pts[i].x));
      pts[i].y = Math.max(30, Math.min(HEIGHT - 30, pts[i].y));
    }
  }

  return pts;
}

/** Interactive concept graph: hover/click a node to highlight its edges. */
export function ConceptGraph({ data }: { data: ConceptGraphData }) {
  const positioned = useMemo(() => layout(data), [data]);
  const [active, setActive] = useState<string | null>(null);

  const posById = useMemo(
    () => new Map(positioned.map((p) => [p.id, p])),
    [positioned],
  );

  if (data.nodes.length === 0) {
    return (
      <div className="banner banner-notice">
        No concepts to graph for this scope yet. Ingest more and synthesize to
        build the concept graph.
      </div>
    );
  }

  const isDimmed = (id: string): boolean =>
    active !== null &&
    active !== id &&
    !data.edges.some(
      (e) =>
        (e.source === active && e.target === id) ||
        (e.target === active && e.source === id),
    );

  return (
    <div className="concept-graph">
      <svg
        viewBox={`0 0 ${WIDTH} ${HEIGHT}`}
        className="concept-graph-svg"
        role="img"
        aria-label="Concept graph"
      >
        <defs>
          {(Object.keys(EDGE_COLORS) as ConceptEdgeKind[]).map((kind) => (
            <marker
              key={kind}
              id={`arrow-${kind}`}
              viewBox="0 0 10 10"
              refX="9"
              refY="5"
              markerWidth="6"
              markerHeight="6"
              orient="auto-start-reverse"
            >
              <path d="M0,0 L10,5 L0,10 z" fill={EDGE_COLORS[kind]} />
            </marker>
          ))}
        </defs>

        {data.edges.map((e, i) => {
          const a = posById.get(e.source);
          const b = posById.get(e.target);
          if (!a || !b) return null;
          const dim =
            active !== null && active !== e.source && active !== e.target;
          return (
            <line
              key={`${e.source}-${e.target}-${i}`}
              x1={a.x}
              y1={a.y}
              x2={b.x}
              y2={b.y}
              stroke={EDGE_COLORS[e.kind]}
              strokeWidth={e.kind === 'contradiction' ? 2 : 1.4}
              strokeDasharray={e.kind === 'supersession' ? '5,4' : undefined}
              markerEnd={`url(#arrow-${e.kind})`}
              opacity={dim ? 0.12 : 0.65}
            />
          );
        })}

        {positioned.map((p) => {
          const dim = isDimmed(p.id);
          return (
            <g
              key={p.id}
              transform={`translate(${p.x},${p.y})`}
              opacity={dim ? 0.25 : 1}
              className="concept-node"
              onMouseEnter={() => setActive(p.id)}
              onMouseLeave={() => setActive((cur) => (cur === p.id ? null : cur))}
              onClick={() => setActive((cur) => (cur === p.id ? null : p.id))}
            >
              <circle
                r={radius(p.weight)}
                fill={nodeColor(p.state)}
                stroke="#0f1419"
                strokeWidth="1.5"
              />
              <title>
                {p.label} — {p.state} ({Math.round(p.weight * 100)}%)
              </title>
              {(active === p.id || data.nodes.length <= 14) && (
                <text
                  x={radius(p.weight) + 4}
                  y="4"
                  className="concept-node-label"
                >
                  {p.label}
                </text>
              )}
            </g>
          );
        })}
      </svg>

      <ConceptGraphLegend />
    </div>
  );
}

function ConceptGraphLegend() {
  return (
    <div className="concept-graph-legend muted small">
      <div className="legend-group">
        <strong>State</strong>
        {Object.entries(STATE_COLORS).map(([state, color]) => (
          <span key={state} className="legend-item">
            <span className="legend-dot" style={{ background: color }} />
            {state}
          </span>
        ))}
      </div>
      <div className="legend-group">
        <strong>Edge</strong>
        {(Object.keys(EDGE_COLORS) as ConceptEdgeKind[]).map((kind) => (
          <span key={kind} className="legend-item">
            <span className="legend-dash" style={{ background: EDGE_COLORS[kind] }} />
            {kind}
          </span>
        ))}
      </div>
    </div>
  );
}
