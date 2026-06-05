'use client';

import { Suspense, useEffect, useMemo, useState } from 'react';
import { useSearchParams } from 'next/navigation';
import { listMemories } from '@/lib/api';
import type { MemoryFilter, MemoryRecord, MemoryState } from '@/lib/types';
import { listConversations, type Conversation } from '@/lib/conversations';
import { isUuid } from '@/lib/format';
import { buildConceptGraph } from '@/lib/concept-graph';
import { Card, ErrorBanner, Notice, PageHeader, Spinner } from '@/components/ui';
import { MemoryCard } from '@/components/MemoryCard';
import { ConceptGraph } from '@/components/ConceptGraph';
import { DecayStateMachine } from '@/components/DecayStateMachine';
import { useAsync } from '@/lib/useAsync';

const FILTERS: { value: '' | MemoryFilter; label: string }[] = [
  { value: '', label: 'All states' },
  { value: 'pinned', label: 'Pinned' },
  { value: 'candidate', label: 'Candidate' },
  { value: 'reinforced', label: 'Reinforced' },
  { value: 'decaying', label: 'Decaying' },
  { value: 'archived', label: 'Archived' },
];

function MemoryBrowser() {
  const params = useSearchParams();
  const initialScope = params?.get('scope') ?? '';

  const [conversations, setConversations] = useState<Conversation[]>([]);
  const [scope, setScope] = useState(initialScope);
  const [filter, setFilter] = useState<'' | MemoryFilter>('');

  // Load the local conversation registry once on mount and default the
  // selected scope to the first conversation when none was supplied via
  // the `?scope=` query param. The functional update reads the latest
  // scope without making it an effect dependency, so this runs exactly
  // once instead of re-running when it sets the scope.
  useEffect(() => {
    const convs = listConversations();
    setConversations(convs);
    setScope((prev) => (prev || (convs[0]?.scopeId ?? '')));
  }, []);

  const valid = isUuid(scope);

  const { data, error, loading } = useAsync<MemoryRecord[]>(
    async (signal) => {
      if (!valid) return [];
      return listMemories(
        scope,
        { filter: filter || undefined, limit: 200 },
        signal,
      );
    },
    [scope, filter, valid],
  );

  const memories = useMemo(() => data ?? [], [data]);

  const counts = useMemo(() => {
    const c: Record<string, number> = {};
    for (const m of memories) {
      const s = String(m.state);
      c[s] = (c[s] ?? 0) + 1;
    }
    return c as Record<MemoryState | string, number>;
  }, [memories]);

  // Graph is built from the unfiltered-by-state set when possible so the
  // relationships stay meaningful; here it uses the currently loaded set.
  const graph = useMemo(() => buildConceptGraph(memories), [memories]);

  return (
    <div className="page">
      <PageHeader
        title="Memory"
        description="Browse synthesized memory by scope, inspect decay states, and explore the derived concept graph."
      />

      <Card>
        <div className="search-form">
          <select
            className="select"
            value={scope}
            onChange={(e) => setScope(e.target.value)}
          >
            {conversations.length === 0 && <option value="">No scopes</option>}
            {conversations.map((c) => (
              <option key={c.scopeId} value={c.scopeId}>
                {c.title}
              </option>
            ))}
          </select>
          <select
            className="select"
            value={filter}
            onChange={(e) => setFilter(e.target.value as '' | MemoryFilter)}
          >
            {FILTERS.map((f) => (
              <option key={f.value || 'all'} value={f.value}>
                {f.label}
              </option>
            ))}
          </select>
        </div>
        {!valid && scope !== '' && (
          <p className="banner banner-error">Scope id is not a valid UUID.</p>
        )}
      </Card>

      <Card title="Decay state machine">
        <DecayStateMachine counts={counts} />
      </Card>

      <Card title="Concept graph (derived)">
        <p className="muted small">
          Nodes are memories (sized by retention, coloured by state); edges are
          lexical-overlap relations between summaries. Archived↔live overlaps
          render as supersession edges.
        </p>
        <ConceptGraph data={graph} />
      </Card>

      <Card title={`Memories${valid ? ` (${memories.length})` : ''}`}>
        <ErrorBanner error={error} />
        {loading && <Spinner label="Loading memory…" />}
        {!loading && valid && memories.length === 0 && !error && (
          <Notice>No memory rows for this scope and filter.</Notice>
        )}
        <div className="memory-grid">
          {memories.map((m) => (
            <MemoryCard key={m.id} memory={m} />
          ))}
        </div>
      </Card>
    </div>
  );
}

export default function MemoryPage() {
  return (
    <Suspense fallback={<Spinner label="Loading…" />}>
      <MemoryBrowser />
    </Suspense>
  );
}
