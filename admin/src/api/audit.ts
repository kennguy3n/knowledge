import { request } from './http';
import type { AuditEvent, AuditQuery } from './types';

/**
 * `GET /api/v1/audit` — query audit events. All filter fields are
 * optional; empty fields are omitted. `limit` defaults to 100 and is
 * capped at 1000 server-side (server/internal/audit).
 */
export function queryAudit(
  q: AuditQuery = {},
  signal?: AbortSignal,
): Promise<AuditEvent[]> {
  return request<AuditEvent[]>('/api/v1/audit', {
    query: {
      tenant_id: q.tenant_id,
      scope_id: q.scope_id,
      action: q.action,
      actor: q.actor,
      from: q.from,
      to: q.to,
      limit: q.limit,
    },
    signal,
  });
}
