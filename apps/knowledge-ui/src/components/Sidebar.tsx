'use client';

import Link from 'next/link';
import { usePathname, useRouter } from 'next/navigation';
import { useCallback, useEffect, useState } from 'react';
import {
  createConversation,
  listConversations,
  type Conversation,
} from '@/lib/conversations';

const NAV = [
  { href: '/', label: 'Conversations', match: (p: string) => p === '/' },
  { href: '/search', label: 'Search', match: (p: string) => p.startsWith('/search') },
  { href: '/memory', label: 'Memory', match: (p: string) => p.startsWith('/memory') },
  { href: '/settings', label: 'Settings', match: (p: string) => p.startsWith('/settings') },
];

export function Sidebar() {
  const pathname = usePathname() ?? '/';
  const router = useRouter();
  const [conversations, setConversations] = useState<Conversation[]>([]);

  const refresh = useCallback(() => setConversations(listConversations()), []);

  useEffect(() => {
    refresh();
    // Reflect changes made in other tabs.
    window.addEventListener('storage', refresh);
    return () => window.removeEventListener('storage', refresh);
  }, [refresh, pathname]);

  function startConversation() {
    const c = createConversation('New conversation');
    refresh();
    router.push(`/chat/${c.scopeId}`);
  }

  const activeScope = pathname.startsWith('/chat/')
    ? decodeURIComponent(pathname.split('/')[2] ?? '')
    : '';

  return (
    <aside className="sidebar">
      <div className="brand">
        <span className="brand-mark">◆</span>
        <span className="brand-name">Knowledge</span>
      </div>

      <nav className="nav-primary">
        {NAV.map((item) => (
          <Link
            key={item.href}
            href={item.href}
            className={item.match(pathname) ? 'nav-link nav-link-active' : 'nav-link'}
          >
            {item.label}
          </Link>
        ))}
      </nav>

      <div className="sidebar-section-head">
        <span>Conversations</span>
        <button className="btn btn-ghost btn-sm" onClick={startConversation}>
          + New
        </button>
      </div>

      <nav className="conversation-list">
        {conversations.length === 0 && (
          <p className="muted small sidebar-empty">No conversations yet.</p>
        )}
        {conversations.map((c) => (
          <Link
            key={c.scopeId}
            href={`/chat/${c.scopeId}`}
            className={
              c.scopeId === activeScope
                ? 'conversation-item conversation-item-active'
                : 'conversation-item'
            }
            title={c.scopeId}
          >
            {c.title}
          </Link>
        ))}
      </nav>
    </aside>
  );
}
