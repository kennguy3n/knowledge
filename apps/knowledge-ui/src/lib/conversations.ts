// Local conversation registry.
//
// The gateway has no "list conversations" endpoint — a scope is just a
// UUID. To give end users a usable conversation list and sidebar, the UI
// keeps a lightweight registry of the scopes the user has interacted
// with (id + human label + last-activity) in localStorage. This is
// purely a client-side convenience index; the authoritative data always
// lives in the gateway/substrate keyed by scope_id.

import { newUuid } from './format';

const STORAGE_KEY = 'knowledge.ui.conversations';

export interface Conversation {
  scopeId: string;
  title: string;
  /** Unix epoch milliseconds of the last local activity. */
  updatedAt: number;
}

function read(): Conversation[] {
  if (typeof window === 'undefined') return [];
  try {
    const raw = window.localStorage.getItem(STORAGE_KEY);
    if (!raw) return [];
    const parsed: unknown = JSON.parse(raw);
    if (!Array.isArray(parsed)) return [];
    return parsed.filter(isConversation);
  } catch {
    return [];
  }
}

function write(items: Conversation[]): void {
  if (typeof window === 'undefined') return;
  try {
    window.localStorage.setItem(STORAGE_KEY, JSON.stringify(items));
  } catch {
    // Storage unavailable — the registry simply does not persist.
  }
}

function isConversation(v: unknown): v is Conversation {
  if (typeof v !== 'object' || v === null) return false;
  const c = v as Conversation;
  return (
    typeof c.scopeId === 'string' &&
    typeof c.title === 'string' &&
    // Guard `updatedAt`: a missing/NaN value would poison the numeric
    // sort comparator in `listConversations`, producing unstable order.
    typeof c.updatedAt === 'number' &&
    Number.isFinite(c.updatedAt)
  );
}

export function listConversations(): Conversation[] {
  return read().sort((a, b) => b.updatedAt - a.updatedAt);
}

export function getConversation(scopeId: string): Conversation | undefined {
  return read().find((c) => c.scopeId === scopeId);
}

/** Insert or update a conversation, bumping its last-activity time. */
export function upsertConversation(
  scopeId: string,
  title?: string,
): Conversation {
  const items = read();
  const now = Date.now();
  const existing = items.find((c) => c.scopeId === scopeId);
  if (existing) {
    if (title) existing.title = title;
    existing.updatedAt = now;
    write(items);
    return existing;
  }
  const created: Conversation = {
    scopeId,
    title: title || 'Untitled conversation',
    updatedAt: now,
  };
  items.push(created);
  write(items);
  return created;
}

export function renameConversation(scopeId: string, title: string): void {
  const items = read();
  const c = items.find((x) => x.scopeId === scopeId);
  if (c) {
    c.title = title;
    write(items);
  }
}

export function removeConversation(scopeId: string): void {
  write(read().filter((c) => c.scopeId !== scopeId));
}

/** Create a new conversation with a fresh scope UUID. */
export function createConversation(title?: string): Conversation {
  return upsertConversation(newUuid(), title);
}
