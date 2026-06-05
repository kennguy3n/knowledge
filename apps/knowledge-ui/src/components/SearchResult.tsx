'use client';

import { useState } from 'react';
import type { EvidenceRecord, QueryResult } from '@/lib/types';
import { getEvidence } from '@/lib/api';
import { formatScore, formatTimestamp } from '@/lib/format';

/**
 * One hybrid-search hit. The query endpoint returns scores + an optional
 * snippet but not the full body, so the full evidence record is fetched
 * lazily on expand via `GET /api/v1/evidence/{id}`.
 */
export function SearchResult({ hit }: { hit: QueryResult }) {
  const [expanded, setExpanded] = useState(false);
  const [record, setRecord] = useState<EvidenceRecord | undefined>();
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | undefined>();

  async function toggle() {
    const next = !expanded;
    setExpanded(next);
    if (next && !record && !loading) {
      setLoading(true);
      setError(undefined);
      try {
        setRecord(await getEvidence(hit.evidence_id));
      } catch (e) {
        setError(e instanceof Error ? e.message : String(e));
      } finally {
        setLoading(false);
      }
    }
  }

  return (
    <div className="search-result">
      <button className="search-result-head" onClick={toggle}>
        <span className="search-result-score">{formatScore(hit.score)}</span>
        <span className="search-result-snippet">
          {hit.snippet || record?.body || hit.evidence_id}
        </span>
        <span className="muted small">{expanded ? '▾' : '▸'}</span>
      </button>

      <div className="search-result-scores muted small">
        <span title="Full-text (FTS5) contribution">
          fts {formatScore(hit.fts_score)}
        </span>
        <span title="Recency contribution">
          recency {formatScore(hit.recency_score)}
        </span>
        <span title="Semantic-vector contribution">
          vector {formatScore(hit.vector_score)}
        </span>
      </div>

      {expanded && (
        <div className="search-result-body">
          {loading && <span className="muted">Loading evidence…</span>}
          {error && <span className="banner banner-error">{error}</span>}
          {record && (
            <>
              <pre className="evidence-body">{record.body}</pre>
              <div className="muted small">
                <span>source {String(record.source)}</span>
                {' · '}
                <span>ingested {formatTimestamp(record.created_at)}</span>
                {record.language_tag && <span> · lang {record.language_tag}</span>}
              </div>
            </>
          )}
        </div>
      )}
    </div>
  );
}
