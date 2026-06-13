'use client';

import { Suspense, useEffect, useMemo, useRef, useState } from 'react';
import { useSearchParams } from 'next/navigation';
import {
  ApiError,
  reasoningContradictions,
  reasoningDrift,
  reasoningExplain,
} from '@/lib/api';
import type {
  ContradictionView,
  DriftReason,
  DriftView,
  QueryExplanationView,
} from '@/lib/types';
import { listConversations, type Conversation } from '@/lib/conversations';
import { formatScore, formatTimestamp, isUuid } from '@/lib/format';
import { Card, ErrorBanner, Notice, PageHeader, Spinner } from '@/components/ui';
import { useAsync } from '@/lib/useAsync';

const DRIFT_REASON_LABEL: Record<DriftReason, string> = {
  evidence_superseded: 'Evidence superseded',
  evidence_removed: 'Evidence removed',
  evidence_weakened: 'Evidence weakened',
};

function driftReasonLabel(reason: string): string {
  return DRIFT_REASON_LABEL[reason as DriftReason] ?? reason;
}

function ReasoningPanel() {
  const params = useSearchParams();
  const initialScope = params?.get('scope') ?? '';

  const [conversations, setConversations] = useState<Conversation[]>([]);
  const [scope, setScope] = useState(initialScope);

  // Load the local conversation registry once on mount and default the
  // selected scope to the first conversation when none was supplied via
  // the `?scope=` query param (mirrors the Memory browser).
  useEffect(() => {
    const convs = listConversations();
    setConversations(convs);
    setScope((prev) => prev || (convs[0]?.scopeId ?? ''));
  }, []);

  const valid = isUuid(scope);

  // ── What contradicts ──────────────────────────────────────────────
  const {
    data: contradictions,
    error: contradictionsError,
    loading: contradictionsLoading,
  } = useAsync<ContradictionView[]>(
    async (signal) => (valid ? reasoningContradictions(scope, signal) : []),
    [scope, valid],
  );

  // ── What changed ──────────────────────────────────────────────────
  const {
    data: drift,
    error: driftError,
    loading: driftLoading,
  } = useAsync<DriftView[]>(
    async (signal) => (valid ? reasoningDrift(scope, signal) : []),
    [scope, valid],
  );

  // useAsync keeps the previous scope's data on error and during the
  // next scope's load window, so guard every render on the *current*
  // selected scope being valid — never render one scope's reasoning
  // results under another scope's heading.
  const contradictionRows = useMemo(
    () => (valid ? (contradictions ?? []) : []),
    [valid, contradictions],
  );
  const driftRows = useMemo(
    () => (valid ? (drift ?? []) : []),
    [valid, drift],
  );

  // ── Why this answer (query-plan explainer) ────────────────────────
  const [queryText, setQueryText] = useState('');
  const [submittedQuery, setSubmittedQuery] = useState('');
  const [explanation, setExplanation] = useState<QueryExplanationView | null>(
    null,
  );
  const [explainError, setExplainError] = useState<string | null>(null);
  const [explaining, setExplaining] = useState(false);

  // The in-flight explain request, so a scope switch can abort it before
  // its response lands. Without this an explain issued under scope A could
  // resolve after the user moved to scope B and overwrite the cleared
  // state, rendering A's plan under B's heading — a cross-scope leak.
  const explainAbortRef = useRef<AbortController | null>(null);

  // A scope switch crosses a tenant boundary: abort any in-flight explain
  // and clear the explainer so one scope's plan never lingers under
  // another scope's heading. The abort makes the old request's promise
  // reject with an AbortError, which `submitExplain` swallows without
  // touching state.
  useEffect(() => {
    explainAbortRef.current?.abort();
    explainAbortRef.current = null;
    setQueryText('');
    setSubmittedQuery('');
    setExplanation(null);
    setExplainError(null);
    setExplaining(false);
  }, [scope]);

  // Abort a still-pending explain when the panel unmounts.
  useEffect(() => () => explainAbortRef.current?.abort(), []);

  const canExplain = valid && queryText.trim() !== '' && !explaining;

  async function submitExplain(e: React.FormEvent) {
    e.preventDefault();
    if (!canExplain) return;
    const q = queryText.trim();
    // Supersede any earlier in-flight explain and track this one so the
    // scope-change / unmount effects can cancel it.
    explainAbortRef.current?.abort();
    const controller = new AbortController();
    explainAbortRef.current = controller;
    setExplaining(true);
    setExplainError(null);
    try {
      const view = await reasoningExplain(scope, q, controller.signal);
      if (controller.signal.aborted) return;
      setExplanation(view);
      setSubmittedQuery(q);
    } catch (err) {
      // A scope switch / unmount / superseding submit aborted this
      // request — its scope context is gone, so drop the result silently.
      if (controller.signal.aborted) return;
      setExplanation(null);
      setExplainError(
        err instanceof ApiError
          ? err.message
          : err instanceof Error
            ? err.message
            : 'Failed to explain query.',
      );
    } finally {
      // Only the current request clears the busy flag; an aborted/superseded
      // request must not re-enable the form under a newer submit's watch.
      if (explainAbortRef.current === controller) {
        explainAbortRef.current = null;
        setExplaining(false);
      }
    }
  }

  return (
    <div className="page">
      <PageHeader
        title="Reasoning"
        description="What changed, what contradicts, and why a given answer was produced — derived by the substrate from this scope’s live memory."
      />

      <Card>
        <div className="search-form">
          <select
            className="select"
            value={scope}
            onChange={(e) => setScope(e.target.value)}
          >
            {conversations.length === 0 && <option value="">No scopes</option>}
            {valid && !conversations.some((c) => c.scopeId === scope) && (
              <option value={scope}>{scope}</option>
            )}
            {conversations.map((c) => (
              <option key={c.scopeId} value={c.scopeId}>
                {c.title}
              </option>
            ))}
          </select>
        </div>
        {!valid && scope !== '' && (
          <p className="banner banner-error">Scope id is not a valid UUID.</p>
        )}
      </Card>

      <Card title="What contradicts">
        <p className="muted small">
          Opposing canonical claims the substrate detected within this scope.
          Each pair is scored by detector confidence.
        </p>
        <ErrorBanner error={contradictionsError} />
        {contradictionsLoading && <Spinner label="Scanning for contradictions…" />}
        {!contradictionsLoading &&
          valid &&
          contradictionRows.length === 0 &&
          !contradictionsError && (
            <Notice>No contradictions detected for this scope.</Notice>
          )}
        {!valid && scope === '' && (
          <Notice>Select a scope to inspect its reasoning.</Notice>
        )}
        <div className="reasoning-list">
          {contradictionRows.map((c) => (
            <div key={c.id} className="reasoning-row">
              <div className="reasoning-claims">
                <span className="reasoning-claim">{c.left_label}</span>
                <span className="reasoning-vs">contradicts</span>
                <span className="reasoning-claim">{c.right_label}</span>
              </div>
              <div className="muted small">
                Confidence {formatScore(c.confidence)} · evidence{' '}
                {c.left_evidence_count} vs {c.right_evidence_count} · detected{' '}
                {formatTimestamp(c.detected_at)}
              </div>
            </div>
          ))}
        </div>
      </Card>

      <Card title="What changed">
        <p className="muted small">
          Canonical claims whose supporting evidence base has shifted —
          superseded, removed, or weakened since the claim was promoted.
        </p>
        <ErrorBanner error={driftError} />
        {driftLoading && <Spinner label="Scanning for drift…" />}
        {!driftLoading && valid && driftRows.length === 0 && !driftError && (
          <Notice>No evidence drift detected for this scope.</Notice>
        )}
        <div className="reasoning-list">
          {driftRows.map((d) => (
            <div key={d.node_id} className="reasoning-row">
              <div className="reasoning-claims">
                <span className="reasoning-claim">{d.label}</span>
                <span className="badge badge-warn">
                  {driftReasonLabel(d.reason)}
                </span>
              </div>
              <div className="muted small">
                Evidence {d.evidence_remaining}/{d.evidence_at_promotion}{' '}
                remaining · detected {formatTimestamp(d.detected_at)}
              </div>
            </div>
          ))}
        </div>
      </Card>

      <Card title="Why this answer">
        <p className="muted small">
          Explain how the substrate would route a question to the cheapest
          satisfying retrieval mode. The plan is a pure function of the query
          text — no scope data is read.
        </p>
        <form className="search-form" onSubmit={submitExplain}>
          <input
            className="input"
            placeholder="A question, e.g. “who approved the vendor change?”"
            value={queryText}
            onChange={(e) => setQueryText(e.target.value)}
            disabled={!valid || explaining}
          />
          <button className="btn btn-primary" type="submit" disabled={!canExplain}>
            {explaining ? 'Explaining…' : 'Explain'}
          </button>
        </form>
        {!valid && scope !== '' && (
          <Notice>Select a valid scope to explain a query.</Notice>
        )}
        {explainError && <p className="banner banner-error">{explainError}</p>}
        {explanation && !explainError && (
          <div className="reasoning-explanation">
            <p>
              Classified <strong>{submittedQuery}</strong> as{' '}
              <span className="badge badge-neutral">{explanation.class}</span>
            </p>
            <p className="reasoning-rationale">{explanation.rationale}</p>
            <ol className="reasoning-plan">
              {explanation.steps.map((s, i) => (
                <li key={`${s.mode}-${i}`}>
                  <span className="reasoning-step-mode">{s.mode}</span>
                  <span className="muted small">
                    {' '}
                    cost rank {s.cost_rank}
                    {typeof s.time_budget_ms === 'number'
                      ? ` · budget ${s.time_budget_ms} ms`
                      : ''}
                  </span>
                </li>
              ))}
            </ol>
          </div>
        )}
      </Card>
    </div>
  );
}

export default function ReasoningPage() {
  return (
    <Suspense fallback={<Spinner label="Loading reasoning…" />}>
      <ReasoningPanel />
    </Suspense>
  );
}
