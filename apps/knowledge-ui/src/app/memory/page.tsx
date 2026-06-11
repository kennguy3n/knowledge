'use client';

import { Suspense, useEffect, useMemo, useState } from 'react';
import { useSearchParams } from 'next/navigation';
import {
  ApiError,
  channelMemory,
  conceptGraph,
  createMemory,
  listMemories,
  pinMemory,
  unpinMemory,
} from '@/lib/api';
import type {
  GraphView,
  Importance,
  MemoryFilter,
  MemoryRecord,
  MemoryState,
} from '@/lib/types';
import { listConversations, type Conversation } from '@/lib/conversations';
import { isUuid } from '@/lib/format';
import { buildConceptGraph, mapGraphView } from '@/lib/concept-graph';
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

const SENSITIVITIES: Importance[] = [
  'Useful',
  'Important',
  'Critical',
  'Noise',
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

  const {
    data,
    error,
    loading,
    reload: reloadMemories,
  } = useAsync<MemoryRecord[]>(
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

  // The synthesised channel recap — the plain-language briefing produced
  // by the most recent synthesis run for this scope. Null until synthesis
  // has run at least once.
  const {
    data: recap,
    loading: recapLoading,
    error: recapError,
  } = useAsync<MemoryRecord | null>(
    async (signal) => (valid ? channelMemory(scope, signal) : null),
    [scope, valid],
  );

  // The concept graph projected server-side from the scope's live
  // user-memory (PR-2 read route). The substrate is the source of truth;
  // if the gateway predates the route (or the request fails) we fall back
  // to the client-derived graph below so the section still renders.
  const {
    data: graphView,
    error: graphError,
    reload: reloadGraph,
  } = useAsync<GraphView | null>(
    async (signal) => (valid ? conceptGraph(scope, signal) : null),
    [scope, valid],
  );

  const counts = useMemo(() => {
    const c: Record<string, number> = {};
    for (const m of memories) {
      const s = String(m.state);
      c[s] = (c[s] ?? 0) + 1;
    }
    return c as Record<MemoryState | string, number>;
  }, [memories]);

  // Size the server graph's nodes by the matching memory's retention
  // score (they share ids) so the graph and the list read consistently.
  const retentionById = useMemo(
    () => new Map(memories.map((m) => [m.id, m.retention_score])),
    [memories],
  );

  // Prefer the server-projected graph; fall back to the client-derived
  // graph when the endpoint is unavailable so the section is never blank
  // on an older gateway. `useAsync` keeps the previous scope's `data` both
  // on error AND during the next scope's loading window, so a plain
  // `Boolean(graphView)` would re-render the previous scope's graph under
  // this scope's heading sized by this scope's retention — a cross-scope
  // leak. The substrate stamps every projection with `scope_filter` =
  // `[scope_id]` (see concept_graph::subgraph_for_scope), so we only trust
  // a `graphView` whose `scope_filter` actually contains the selected
  // scope. That fails closed for both the error case and the transient
  // stale-data-during-load case in one check.
  const hasServerGraph =
    !graphError &&
    graphView != null &&
    graphView.scope_filter.includes(scope);
  const graph = useMemo(() => {
    if (hasServerGraph && graphView) return mapGraphView(graphView, retentionById);
    return buildConceptGraph(memories);
  }, [hasServerGraph, graphView, memories, retentionById]);
  // Only flag the explicit-fallback notice on a real error, not while the
  // current scope's graph is still loading (no error yet, no usable view).
  const graphFallback = !hasServerGraph && Boolean(graphError);

  // ── Create-memory affordance ──────────────────────────────────────
  const [obsType, setObsType] = useState('');
  const [content, setContent] = useState('');
  const [sensitivity, setSensitivity] = useState<Importance>('Useful');
  const [submitting, setSubmitting] = useState(false);
  const [formError, setFormError] = useState<string | null>(null);
  const [formNotice, setFormNotice] = useState<string | null>(null);

  // Which memory ids are mid pin/unpin, so each control can show its own
  // busy state without disabling the whole list. A Set (not a single id)
  // so two rapid clicks on different rows don't clear each other's spinner.
  const [pinningIds, setPinningIds] = useState<ReadonlySet<string>>(
    () => new Set(),
  );
  const [actionError, setActionError] = useState<string | null>(null);

  // Transient banners ("Memory written.", write/pin errors) describe an
  // action taken against the previously selected scope, so clear them when
  // the scope changes — otherwise a success/error notice would linger under
  // a different scope's form and misattribute the outcome.
  useEffect(() => {
    setFormNotice(null);
    setFormError(null);
    setActionError(null);
  }, [scope]);

  const canSubmit =
    valid && obsType.trim() !== '' && content.trim() !== '' && !submitting;

  async function submitCreate(e: React.FormEvent) {
    e.preventDefault();
    if (!canSubmit) return;
    setSubmitting(true);
    setFormError(null);
    setFormNotice(null);
    try {
      await createMemory({
        scope_id: scope,
        observation_type: obsType.trim(),
        content: content.trim(),
        sensitivity,
      });
      setContent('');
      setObsType('');
      setFormNotice('Memory written.');
      reloadMemories();
      reloadGraph();
    } catch (err) {
      setFormError(
        err instanceof ApiError
          ? err.message
          : err instanceof Error
            ? err.message
            : 'Failed to write memory.',
      );
    } finally {
      setSubmitting(false);
    }
  }

  async function togglePin(memory: MemoryRecord) {
    setPinningIds((prev) => new Set(prev).add(memory.id));
    setActionError(null);
    try {
      if (String(memory.state).toLowerCase() === 'pinned') {
        await unpinMemory(memory.id);
      } else {
        await pinMemory(memory.id);
      }
      reloadMemories();
      reloadGraph();
    } catch (err) {
      setActionError(
        err instanceof Error ? err.message : 'Failed to update pin state.',
      );
    } finally {
      setPinningIds((prev) => {
        const next = new Set(prev);
        next.delete(memory.id);
        return next;
      });
    }
  }

  return (
    <div className="page">
      <PageHeader
        title="Memory"
        description="Browse synthesized memory by scope, inspect decay states, and explore the concept graph projected from live memory."
      />

      <Card>
        <div className="search-form">
          <select
            className="select"
            value={scope}
            onChange={(e) => setScope(e.target.value)}
          >
            {conversations.length === 0 && <option value="">No scopes</option>}
            {/* A scope supplied via `?scope=` (e.g. a shared link) may not be
                in the local registry; surface it so the dropdown reflects the
                scope whose memory is actually loaded. */}
            {valid && !conversations.some((c) => c.scopeId === scope) && (
              <option value={scope}>{scope}</option>
            )}
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

      <Card title="Add a memory">
        <p className="muted small">
          Write a user-memory observation for this scope. It enters the decay
          state machine as a <code>Candidate</code> and shows up below and in
          the concept graph immediately.
        </p>
        <form className="memory-form" onSubmit={submitCreate}>
          <div className="memory-form-row">
            <input
              className="input"
              placeholder="Observation type (e.g. preference, fact, decision)"
              value={obsType}
              onChange={(e) => setObsType(e.target.value)}
              disabled={!valid || submitting}
            />
            <select
              className="select"
              value={sensitivity}
              onChange={(e) => setSensitivity(e.target.value as Importance)}
              disabled={!valid || submitting}
            >
              {SENSITIVITIES.map((s) => (
                <option key={s} value={s}>
                  {s}
                </option>
              ))}
            </select>
          </div>
          <textarea
            className="textarea"
            placeholder="What should be remembered for this scope?"
            value={content}
            onChange={(e) => setContent(e.target.value)}
            disabled={!valid || submitting}
            rows={3}
          />
          <div className="memory-form-actions">
            <button className="btn btn-primary" type="submit" disabled={!canSubmit}>
              {submitting ? 'Writing…' : 'Write memory'}
            </button>
          </div>
        </form>
        {!valid && scope !== '' && (
          <Notice>Select a valid scope to write a memory.</Notice>
        )}
        {formError && <p className="banner banner-error">{formError}</p>}
        {formNotice && !formError && (
          <p className="banner banner-notice">{formNotice}</p>
        )}
      </Card>

      <Card title="Synthesized briefing">
        <p className="muted small">
          The plain-language recap produced by the most recent synthesis run for
          this scope — raw evidence condensed into a briefing.
        </p>
        <ErrorBanner error={recapError} />
        {recapLoading && <Spinner label="Loading briefing…" />}
        {!recapLoading && valid && !recap && !recapError && (
          <Notice>
            No briefing yet. Trigger synthesis for this scope to generate one.
          </Notice>
        )}
        {recap && <p className="synthesis-recap">{recap.summary}</p>}
      </Card>

      <Card title="Decay state machine">
        <DecayStateMachine counts={counts} />
      </Card>

      <Card title="Concept graph">
        <p className="muted small">
          {graphFallback
            ? 'Showing a client-derived graph (the server concept-graph route is unavailable for this gateway). Nodes are memories; edges are lexical-overlap relations.'
            : 'Projected by the substrate from this scope’s live user-memory: nodes are observations coloured by lifecycle state and sized by retention; supersession pointers render as supersession edges.'}
        </p>
        <ConceptGraph data={graph} />
      </Card>

      <Card title={`Memories${valid ? ` (${memories.length})` : ''}`}>
        <ErrorBanner error={error} />
        {actionError && <p className="banner banner-error">{actionError}</p>}
        {loading && <Spinner label="Loading memory…" />}
        {!loading && valid && memories.length === 0 && !error && (
          <Notice>No memory rows for this scope and filter.</Notice>
        )}
        <div className="memory-grid">
          {memories.map((m) => (
            <MemoryCard
              key={m.id}
              memory={m}
              onTogglePin={togglePin}
              pinBusy={pinningIds.has(m.id)}
            />
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
