import { request } from './http';
import type { SynthesisRecord, SynthesisTriggerRequest } from './types';

const BASE = '/api/v1/synthesis';

/** `POST /api/v1/synthesis/trigger` — kick off a synthesis run. */
export function triggerSynthesis(
  body: SynthesisTriggerRequest,
): Promise<SynthesisRecord> {
  return request<SynthesisRecord>(`${BASE}/trigger`, { method: 'POST', body });
}

/**
 * `GET /api/v1/synthesis/recent?scope_id=…` — recent synthesis runs
 * for a scope. The substrate returns an opaque array; we type it as
 * `SynthesisRecord[]` and render defensively.
 */
export function recentSyntheses(
  scopeId: string,
  signal?: AbortSignal,
): Promise<SynthesisRecord[]> {
  return request<SynthesisRecord[]>(`${BASE}/recent`, {
    query: { scope_id: scopeId },
    signal,
  });
}

/**
 * `GET /api/v1/synthesis/{id}/status` — single-snapshot status of a
 * synthesis run. (The gateway can also stream via SSE when
 * `Accept: text/event-stream`; the admin uses the JSON snapshot.)
 */
export function synthesisStatus(
  id: string,
  signal?: AbortSignal,
): Promise<SynthesisRecord> {
  return request<SynthesisRecord>(
    `${BASE}/${encodeURIComponent(id)}/status`,
    { signal },
  );
}
