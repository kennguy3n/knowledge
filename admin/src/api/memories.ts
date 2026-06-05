import { request } from './http';
import type { MemoryFilter, MemoryRecord } from './types';

/**
 * `GET /api/v1/memories?scope_id=…` — list memory objects for a scope.
 * `filter` is either a decay state ("candidate" | "reinforced" |
 * "archived") or the special "pinned" value (see
 * server/internal/gateway/evidence.go listMemories).
 */
export function listMemories(
  scopeId: string,
  opts: { filter?: MemoryFilter; limit?: number } = {},
  signal?: AbortSignal,
): Promise<MemoryRecord[]> {
  return request<MemoryRecord[]>('/api/v1/memories', {
    query: { scope_id: scopeId, filter: opts.filter, limit: opts.limit },
    signal,
  });
}

/**
 * `POST /api/v1/forget/{scope_id}` — cryptographically forget an
 * entire scope (irreversible DEK destruction). Returns 204.
 */
export function forgetScope(scopeId: string): Promise<void> {
  return request<void>(`/api/v1/forget/${encodeURIComponent(scopeId)}`, {
    method: 'POST',
  });
}
