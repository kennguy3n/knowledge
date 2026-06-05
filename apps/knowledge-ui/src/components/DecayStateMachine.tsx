import type { MemoryState } from '@/lib/types';

// Visualises the memory decay lifecycle as a left-to-right pipeline with
// a live count per state. The wire enum the substrate persists is the
// five states below (crates/ffi/src/types.rs MemoryState); Pinned is
// shown as a decay-immune side state.
const FLOW: { state: MemoryState; blurb: string }[] = [
  { state: 'Candidate', blurb: 'Newly observed; awaiting reinforcement.' },
  { state: 'Reinforced', blurb: 'Confirmed by reuse; full retention.' },
  { state: 'Decaying', blurb: 'Ageing toward archival.' },
  { state: 'Archived', blurb: 'Cold-archived, encrypted at rest.' },
];

const STATE_TONE: Record<string, string> = {
  candidate: 'warn',
  reinforced: 'ok',
  decaying: 'warn',
  archived: 'bad',
  pinned: 'accent',
};

export function DecayStateMachine({
  counts,
}: {
  counts: Record<string, number>;
}) {
  const lookup = (s: string) => counts[s] ?? counts[s.toLowerCase()] ?? 0;

  return (
    <div className="decay-machine">
      <div className="decay-flow">
        {FLOW.map((node, i) => (
          <div key={node.state} className="decay-node-wrap">
            <div
              className={`decay-node decay-${STATE_TONE[node.state.toLowerCase()]}`}
              title={node.blurb}
            >
              <span className="decay-count">{lookup(node.state)}</span>
              <span className="decay-name">{node.state}</span>
            </div>
            {i < FLOW.length - 1 && <span className="decay-arrow">→</span>}
          </div>
        ))}
      </div>
      <div className="decay-pinned">
        <div className="decay-node decay-accent" title="Pinned by user — decay-immune.">
          <span className="decay-count">{lookup('Pinned')}</span>
          <span className="decay-name">Pinned</span>
        </div>
        <span className="muted small">decay-immune</span>
      </div>
    </div>
  );
}
