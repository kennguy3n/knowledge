'use client';

import { useRouter } from 'next/navigation';
import Link from 'next/link';
import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { channelMemory, forgetScope, ingest, listMemories } from '@/lib/api';
import type { Importance, MemoryRecord } from '@/lib/types';
import { isUuid, newUuid } from '@/lib/format';
import {
  getConversation,
  removeConversation,
  renameConversation,
  upsertConversation,
} from '@/lib/conversations';
import { useScopeId } from '@/lib/useScopeId';
import { ChatMessage, MessageBubble } from '@/components/MessageBubble';
import { MemoryCard } from '@/components/MemoryCard';
import { SynthesisStatus } from '@/components/SynthesisStatus';
import { ErrorBanner, Notice, Spinner } from '@/components/ui';

const IMPORTANCE_OPTIONS: Importance[] = [
  'Critical',
  'Important',
  'Useful',
  'Noise',
];

export function ChatView() {
  const router = useRouter();
  const scopeId = useScopeId();

  const [title, setTitle] = useState('Conversation');
  const [messages, setMessages] = useState<ChatMessage[]>([]);
  const [draft, setDraft] = useState('');
  const [importance, setImportance] = useState<Importance>('Useful');
  const [sendError, setSendError] = useState<Error | undefined>();
  const [confirmForget, setConfirmForget] = useState(false);
  const [forgetting, setForgetting] = useState(false);

  const [memories, setMemories] = useState<MemoryRecord[]>([]);
  const [recap, setRecap] = useState<MemoryRecord | null>(null);
  const [memLoading, setMemLoading] = useState(false);
  const [memError, setMemError] = useState<Error | undefined>();

  const listRef = useRef<HTMLDivElement>(null);

  // Register/refresh the conversation in the local registry.
  useEffect(() => {
    if (!scopeId || !isUuid(scopeId)) return;
    const existing = getConversation(scopeId);
    const c = existing ?? upsertConversation(scopeId, `Scope ${scopeId.slice(0, 8)}`);
    setTitle(c.title);
  }, [scopeId]);

  const refreshMemories = useCallback(
    async (signal?: AbortSignal) => {
      if (!scopeId || !isUuid(scopeId)) return;
      setMemLoading(true);
      setMemError(undefined);
      try {
        // The panel reflects two distinct synthesis surfaces: the channel
        // recap (the briefing produced by "Synthesize now") and the
        // per-item user memories. Fetch both so a freshly synthesized
        // briefing actually appears here instead of "No memory yet".
        //
        // The recap is supplementary: `listMemories` is the primary
        // content, so a recap fetch that fails for any reason other than
        // "not synthesized yet" (which already resolves to null) must not
        // blank out the list. Degrade the recap to null on failure and let
        // `listMemories` alone drive the panel's error/loading state.
        const [recapRow, rows] = await Promise.all([
          channelMemory(scopeId, signal).catch((err: unknown) => {
            // Degrade to null, but don't let the failure vanish entirely:
            // log it so a channel-memory-only outage is still discoverable
            // while debugging (aborts are expected during navigation).
            if (!signal?.aborted) {
              console.warn('channel recap fetch failed; omitting recap', err);
            }
            return null;
          }),
          listMemories(scopeId, { limit: 50 }, signal),
        ]);
        if (!signal?.aborted) {
          setRecap(recapRow);
          setMemories(rows);
        }
      } catch (e) {
        if (!signal?.aborted) {
          setMemError(e instanceof Error ? e : new Error(String(e)));
        }
      } finally {
        if (!signal?.aborted) setMemLoading(false);
      }
    },
    [scopeId],
  );

  useEffect(() => {
    const controller = new AbortController();
    void refreshMemories(controller.signal);
    return () => controller.abort();
  }, [refreshMemories]);

  // Auto-scroll to the newest message.
  useEffect(() => {
    listRef.current?.scrollTo({ top: listRef.current.scrollHeight });
  }, [messages]);

  async function send(e: React.FormEvent) {
    e.preventDefault();
    const body = draft.trim();
    if (!body || !scopeId) return;
    if (!isUuid(scopeId)) {
      setSendError(new Error('This conversation has an invalid scope id.'));
      return;
    }

    const localId = newUuid();
    const pending: ChatMessage = {
      id: localId,
      role: 'user',
      body,
      at: Date.now(),
      pending: true,
    };
    setMessages((prev) => [...prev, pending]);
    setDraft('');
    setSendError(undefined);

    // First message names an untitled conversation.
    if (messages.length === 0) {
      const t = body.length > 40 ? `${body.slice(0, 39)}…` : body;
      renameConversation(scopeId, t);
      setTitle(t);
    } else {
      upsertConversation(scopeId);
    }

    try {
      const { id } = await ingest({
        scope_id: scopeId,
        body,
        source: 'Manual',
        importance,
      });
      setMessages((prev) =>
        prev.map((m) =>
          m.id === localId ? { ...m, pending: false, evidenceId: id } : m,
        ),
      );
    } catch (err) {
      setMessages((prev) =>
        prev.map((m) =>
          m.id === localId ? { ...m, pending: false, error: true } : m,
        ),
      );
      setSendError(err instanceof Error ? err : new Error(String(err)));
    }
  }

  async function doForget() {
    if (!scopeId) return;
    setForgetting(true);
    try {
      await forgetScope(scopeId);
      removeConversation(scopeId);
      router.push('/');
    } catch (err) {
      setSendError(err instanceof Error ? err : new Error(String(err)));
      setForgetting(false);
      setConfirmForget(false);
    }
  }

  const invalidScope = useMemo(
    () => Boolean(scopeId) && !isUuid(scopeId),
    [scopeId],
  );

  if (!scopeId) {
    return <Spinner label="Resolving conversation…" />;
  }

  return (
    <div className="chat-layout">
      <section className="chat-main">
        <header className="chat-header">
          <div>
            <h1>{title}</h1>
            <p className="muted small mono">{scopeId}</p>
          </div>
          <div className="chat-header-actions">
            <Link className="btn btn-ghost btn-sm" href={`/memory?scope=${scopeId}`}>
              View memory
            </Link>
            <button
              className="btn btn-danger btn-sm"
              onClick={() => setConfirmForget(true)}
            >
              Forget conversation
            </button>
          </div>
        </header>

        {invalidScope && (
          <Notice>
            This conversation’s scope id is not a valid UUID, so the gateway
            will reject requests for it.
          </Notice>
        )}

        <div className="chat-messages" ref={listRef}>
          {messages.length === 0 ? (
            <Notice>
              Send a message to ingest it as evidence into this scope. Use the
              memory panel to synthesize and review what the system remembers.
            </Notice>
          ) : (
            messages.map((m) => <MessageBubble key={m.id} message={m} />)
          )}
        </div>

        <ErrorBanner error={sendError} />

        <form className="chat-composer" onSubmit={send}>
          <select
            className="select"
            value={importance}
            onChange={(e) => setImportance(e.target.value as Importance)}
            title="Importance — influences retention"
          >
            {IMPORTANCE_OPTIONS.map((opt) => (
              <option key={opt} value={opt}>
                {opt}
              </option>
            ))}
          </select>
          <textarea
            className="textarea"
            placeholder="Type a message to ingest…"
            value={draft}
            rows={2}
            onChange={(e) => setDraft(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === 'Enter' && !e.shiftKey) {
                e.preventDefault();
                void send(e as unknown as React.FormEvent);
              }
            }}
          />
          <button className="btn btn-primary" type="submit" disabled={!draft.trim()}>
            Send
          </button>
        </form>
      </section>

      <aside className="memory-panel">
        <div className="memory-panel-head">
          <h2>Synthesized memory</h2>
          <button
            className="btn btn-ghost btn-sm"
            onClick={() => void refreshMemories()}
            disabled={memLoading}
          >
            Refresh
          </button>
        </div>

        <SynthesisStatus scopeId={scopeId} onComplete={() => void refreshMemories()} />

        <ErrorBanner error={memError} />
        {memLoading && <Spinner label="Loading memory…" />}
        {/* Treat an empty/whitespace recap as "no recap": a token-capped
            synthesis can be salvaged into an empty summary, which must fall
            through to the empty state rather than render a blank paragraph. */}
        {!memLoading && !memError && recap && recap.summary.trim() !== '' && (
          <p className="synthesis-recap">{recap.summary}</p>
        )}
        {!memLoading &&
          !memError &&
          (!recap || recap.summary.trim() === '') &&
          memories.length === 0 && (
            <Notice>
              No memory yet for this scope. Ingest a few messages, then
              “Synthesize now” to condense them into a briefing.
            </Notice>
          )}
        <div className="memory-panel-list">
          {memories.map((m) => (
            <MemoryCard key={m.id} memory={m} />
          ))}
        </div>
      </aside>

      {confirmForget && (
        <ForgetDialog
          busy={forgetting}
          onCancel={() => setConfirmForget(false)}
          onConfirm={doForget}
        />
      )}
    </div>
  );
}

function ForgetDialog({
  busy,
  onCancel,
  onConfirm,
}: {
  busy: boolean;
  onCancel: () => void;
  onConfirm: () => void;
}) {
  return (
    <div className="modal-overlay" role="dialog" aria-modal="true">
      <div className="modal">
        <h2>Forget this conversation?</h2>
        <p>
          This calls <code>POST /api/v1/forget</code>, which{' '}
          <strong>cryptographically destroys the scope’s encryption key</strong>.
          All evidence and synthesized memory for this scope become
          permanently unrecoverable. This action <strong>cannot be undone</strong>.
        </p>
        <div className="modal-actions">
          <button className="btn" onClick={onCancel} disabled={busy}>
            Cancel
          </button>
          <button className="btn btn-danger" onClick={onConfirm} disabled={busy}>
            {busy ? 'Forgetting…' : 'Forget permanently'}
          </button>
        </div>
      </div>
    </div>
  );
}
