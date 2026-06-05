import type { MemoryRecord } from '@/lib/types';
import { formatScore, formatTimestamp } from '@/lib/format';
import { StatusBadge } from './ui';

/** A single synthesized-memory row. */
export function MemoryCard({ memory }: { memory: MemoryRecord }) {
  return (
    <div className="memory-card">
      <div className="memory-card-head">
        <StatusBadge status={String(memory.state)} />
        <span className="muted small">
          retention {formatScore(memory.retention_score)}
        </span>
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
