import { useState } from 'react';
import { memoriesApi } from '../api';
import type { MemoryFilter, MemoryRecord } from '../api';
import {
  Card,
  ErrorBanner,
  JsonBlock,
  Notice,
  PageHeader,
  Spinner,
  StatusBadge,
} from '../components/ui';

const FILTERS: { value: '' | MemoryFilter; label: string }[] = [
  { value: '', label: 'all' },
  { value: 'pinned', label: 'pinned' },
  { value: 'candidate', label: 'candidate' },
  { value: 'reinforced', label: 'reinforced' },
  { value: 'archived', label: 'archived' },
];

export default function Memory() {
  const [scopeId, setScopeId] = useState('');
  const [filter, setFilter] = useState<'' | MemoryFilter>('');
  const [limit, setLimit] = useState(50);
  const [rows, setRows] = useState<MemoryRecord[] | undefined>();
  const [error, setError] = useState<Error | undefined>();
  const [loading, setLoading] = useState(false);
  const [inspect, setInspect] = useState<MemoryRecord | undefined>();

  async function onLoad(e: React.FormEvent) {
    e.preventDefault();
    setLoading(true);
    setError(undefined);
    setInspect(undefined);
    try {
      setRows(
        await memoriesApi.listMemories(scopeId.trim(), {
          filter: filter || undefined,
          limit,
        }),
      );
    } catch (err) {
      setError(err instanceof Error ? err : new Error(String(err)));
    } finally {
      setLoading(false);
    }
  }

  return (
    <div>
      <PageHeader
        title="Memory browser"
        description="Browse decaying memory objects for a scope by decay state."
      />

      <ErrorBanner error={error} />

      <Card title="Query">
        <form onSubmit={onLoad}>
          <div className="row">
            <div className="field">
              <label>Scope ID (UUID)</label>
              <input
                value={scopeId}
                onChange={(e) => setScopeId(e.target.value)}
                placeholder="00000000-0000-0000-0000-000000000000"
                required
              />
            </div>
            <div className="field">
              <label>Filter</label>
              <select
                value={filter}
                onChange={(e) => setFilter(e.target.value as '' | MemoryFilter)}
              >
                {FILTERS.map((f) => (
                  <option key={f.label} value={f.value}>
                    {f.label}
                  </option>
                ))}
              </select>
            </div>
            <div className="field">
              <label>Limit</label>
              <input
                type="number"
                min={1}
                value={limit}
                onChange={(e) => setLimit(Number(e.target.value))}
              />
            </div>
            <div className="field row-fixed">
              <label>&nbsp;</label>
              <button type="submit" disabled={loading || !scopeId.trim()}>
                Load
              </button>
            </div>
          </div>
        </form>
      </Card>

      <Card title="Memories">
        {loading && <Spinner />}
        {!loading && rows && rows.length === 0 && (
          <Notice>No memories match this query.</Notice>
        )}
        {rows && rows.length > 0 && (
          <table>
            <thead>
              <tr>
                <th>ID</th>
                <th>State</th>
                <th>Pinned</th>
                <th>Summary</th>
                <th></th>
              </tr>
            </thead>
            <tbody>
              {rows.map((m, i) => (
                <tr key={str(m.id) ?? i}>
                  <td className="mono">{str(m.id) ?? '—'}</td>
                  <td>{m.state ? <StatusBadge status={String(m.state)} /> : '—'}</td>
                  <td>{m.pinned ? 'yes' : 'no'}</td>
                  <td>{str(m.summary) ?? '—'}</td>
                  <td>
                    <button className="btn-sm" onClick={() => setInspect(m)}>
                      Inspect
                    </button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </Card>

      {inspect && (
        <Card title="Memory detail">
          <JsonBlock value={inspect} />
        </Card>
      )}
    </div>
  );
}

function str(v: unknown): string | undefined {
  return typeof v === 'string' ? v : undefined;
}
