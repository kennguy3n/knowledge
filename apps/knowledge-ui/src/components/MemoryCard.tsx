import type { MemoryRecord } from '@/lib/types';
import { formatScore, formatTimestamp } from '@/lib/format';
import { StatusBadge } from './ui';

/** True when a memory is pinned (decay-immune). */
function isPinned(memory: MemoryRecord): boolean {
  return String(memory.state).toLowerCase() === 'pinned';
}

/**
 * A single synthesized-memory row. When `onTogglePin` is supplied a
 * pin/unpin control is rendered, letting the operator hold a memory
 * decay-immune (or release it) directly from the list.
 */
export function MemoryCard({
  memory,
  onTogglePin,
  pinBusy = false,
}: {
  memory: MemoryRecord;
  onTogglePin?: (memory: MemoryRecord) => void;
  pinBusy?: boolean;
}) {
  const pinned = isPinned(memory);
  return (
    <div className="memory-card">
      <div className="memory-card-head">
        <StatusBadge status={String(memory.state)} />
        <span className="muted small">
          retention {formatScore(memory.retention_score)}
        </span>
        {onTogglePin && (
          <button
            type="button"
            className="btn btn-ghost btn-sm memory-card-pin"
            onClick={() => onTogglePin(memory)}
            disabled={pinBusy}
            aria-pressed={pinned}
            title={
              pinned
                ? 'Release this memory so it resumes normal decay'
                : 'Pin this memory so the decay state machine never archives it'
            }
          >
            {pinned ? 'Unpin' : 'Pin'}
          </button>
        )}
      </div>
      <p className="memory-card-summary">{memory.summary || '(no summary)'}</p>
      <div className="memory-card-meta muted small">
        <span>created {formatTimestamp(memory.created_at)}</span>
        <span>reinforced {formatTimestamp(memory.last_reinforced_at)}</span>
      </div>
      {/* Retention bar visualises the decay score. */}
      <div className="meter" aria-hidden="true">
        <div
          className="meter-fill"
          style={{
            width: `${Math.max(0, Math.min(1, memory.retention_score)) * 100}%`,
          }}
        />
      </div>
    </div>
  );
}
