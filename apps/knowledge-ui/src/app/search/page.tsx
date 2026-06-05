'use client';

import Link from 'next/link';
import { useEffect, useMemo, useState } from 'react';
import { query } from '@/lib/api';
import type { QueryResult } from '@/lib/types';
import { listConversations, type Conversation } from '@/lib/conversations';
import { isUuid } from '@/lib/format';
import { Card, ErrorBanner, Notice, PageHeader, Spinner } from '@/components/ui';
import { SearchResult } from '@/components/SearchResult';

interface ScopedHit {
  scopeId: string;
  scopeTitle: string;
  hit: QueryResult;
}

const ALL = '__all__';

export default function SearchPage() {
  const [conversations, setConversations] = useState<Conversation[]>([]);
  const [scope, setScope] = useState<string>(ALL);
  const [text, setText] = useState('');
  const [results, setResults] = useState<ScopedHit[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<Error | undefined>();
  const [searched, setSearched] = useState(false);

  useEffect(() => {
    setConversations(listConversations());
  }, []);

  const targets = useMemo<Conversation[]>(() => {
    if (scope === ALL) return conversations.filter((c) => isUuid(c.scopeId));
    const one = conversations.find((c) => c.scopeId === scope);
    return one ? [one] : [];
  }, [scope, conversations]);

  async function run(e: React.FormEvent) {
    e.preventDefault();
    const q = text.trim();
    if (!q) return;
    setLoading(true);
    setError(undefined);
    setSearched(true);
    try {
      // Fan out across the selected scope(s); the gateway query endpoint
      // is per-scope, so "all" merges results client-side by score.
      const settled = await Promise.allSettled(
        targets.map(async (c) => ({
          conversation: c,
          hits: await query({ scope_id: c.scopeId, query_text: q, limit: 20 }),
        })),
      );
      const merged: ScopedHit[] = [];
      const errors: string[] = [];
      for (const r of settled) {
        if (r.status === 'fulfilled') {
          for (const hit of r.value.hits) {
            merged.push({
              scopeId: r.value.conversation.scopeId,
              scopeTitle: r.value.conversation.title,
              hit,
            });
          }
        } else {
          errors.push(String(r.reason));
        }
      }
      merged.sort((a, b) => b.hit.score - a.hit.score);
      setResults(merged);
      if (merged.length === 0 && errors.length > 0) {
        setError(new Error(errors[0]));
      }
    } catch (err) {
      setError(err instanceof Error ? err : new Error(String(err)));
    } finally {
      setLoading(false);
    }
  }

  return (
    <div className="page">
      <PageHeader
        title="Search"
        description="Hybrid full-text + semantic search over the evidence in your scopes."
      />

      <Card>
        <form className="search-form" onSubmit={run}>
          <select
            className="select"
            value={scope}
            onChange={(e) => setScope(e.target.value)}
          >
            <option value={ALL}>All conversations</option>
            {conversations.map((c) => (
              <option key={c.scopeId} value={c.scopeId}>
                {c.title}
              </option>
            ))}
          </select>
          <input
            className="input"
            placeholder="Search query (FTS5 syntax supported)…"
            value={text}
            onChange={(e) => setText(e.target.value)}
          />
          <button className="btn btn-primary" type="submit" disabled={loading}>
            Search
          </button>
        </form>
        {targets.length === 0 && (
          <p className="muted small">
            No searchable scopes yet — start a conversation first.
          </p>
        )}
      </Card>

      <ErrorBanner error={error} />
      {loading && <Spinner label="Searching…" />}

      {!loading && searched && results.length === 0 && !error && (
        <Notice>No results.</Notice>
      )}

      {results.length > 0 && (
        <div className="search-results">
          {results.map((r, i) => (
            <div key={`${r.scopeId}-${r.hit.evidence_id}-${i}`} className="scoped-hit">
              {scope === ALL && (
                <Link href={`/chat/${r.scopeId}`} className="scoped-hit-scope muted small">
                  {r.scopeTitle}
                </Link>
              )}
              <SearchResult hit={r.hit} />
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
