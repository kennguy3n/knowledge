'use client';

import Link from 'next/link';
import { useRouter } from 'next/navigation';
import { useCallback, useEffect, useState } from 'react';
import {
  createConversation,
  listConversations,
  removeConversation,
  upsertConversation,
  type Conversation,
} from '@/lib/conversations';
import { isUuid } from '@/lib/format';
import { formatTimestamp } from '@/lib/format';
import { Card, Notice, PageHeader } from '@/components/ui';

export default function HomePage() {
  const router = useRouter();
  const [conversations, setConversations] = useState<Conversation[]>([]);
  const [openId, setOpenId] = useState('');
  const [openError, setOpenError] = useState<string | undefined>();

  const refresh = useCallback(() => setConversations(listConversations()), []);

  useEffect(() => {
    refresh();
  }, [refresh]);

  function startConversation() {
    const c = createConversation('New conversation');
    router.push(`/chat/${c.scopeId}`);
  }

  function openExisting(e: React.FormEvent) {
    e.preventDefault();
    const id = openId.trim();
    if (!isUuid(id)) {
      setOpenError('Enter a valid scope UUID.');
      return;
    }
    upsertConversation(id, `Scope ${id.slice(0, 8)}`);
    router.push(`/chat/${id}`);
  }

  function remove(scopeId: string) {
    removeConversation(scopeId);
    refresh();
  }

  return (
    <div className="page">
      <PageHeader
        title="Conversations"
        description="Chat with your knowledge base. Each conversation is an isolated scope; messages you send are ingested as evidence and synthesized into memory."
        actions={
          <button className="btn btn-primary" onClick={startConversation}>
            + New conversation
          </button>
        }
      />

      <Card title="Open an existing scope">
        <form className="inline-form" onSubmit={openExisting}>
          <input
            className="input"
            placeholder="scope UUID (e.g. 123e4567-e89b-12d3-a456-426614174000)"
            value={openId}
            onChange={(e) => {
              setOpenId(e.target.value);
              setOpenError(undefined);
            }}
          />
          <button className="btn" type="submit">
            Open
          </button>
        </form>
        {openError && <p className="banner banner-error">{openError}</p>}
      </Card>

      {conversations.length === 0 ? (
        <Notice>
          No conversations yet. Start a new conversation or open an existing
          scope by its UUID.
        </Notice>
      ) : (
        <div className="conversation-grid">
          {conversations.map((c) => (
            <div key={c.scopeId} className="conversation-card">
              <Link href={`/chat/${c.scopeId}`} className="conversation-card-main">
                <h3>{c.title}</h3>
                <p className="muted small mono">{c.scopeId}</p>
                <p className="muted small">
                  updated {formatTimestamp(Math.round(c.updatedAt / 1000))}
                </p>
              </Link>
              <button
                className="btn btn-ghost btn-sm"
                onClick={() => remove(c.scopeId)}
                title="Remove from this device (does not delete data on the server)"
              >
                Remove
              </button>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
