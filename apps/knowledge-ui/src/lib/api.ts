// Typed client wrapping the Knowledge gateway REST surface.
//
// Base URL resolution:
//   - Default is "" (same origin). In the nginx production image the
//     static UI is served on the same origin as a reverse-proxied
//     gateway, and `next dev` is expected to run behind the same proxy
//     or against a same-origin gateway, so "" works in both.
//   - Override at build time with NEXT_PUBLIC_GATEWAY_BASE_URL for
//     setups where the gateway is on a different origin (CORS must then
//     be enabled gateway-side via KNOWLEDGE_CORS_ORIGINS).
//
// Auth: the gateway accepts a static API key or a tenant JWT as a
// Bearer token (server/internal/middleware/middleware.go). The token is
// read from localStorage so an end user can paste it in via Settings;
// it is never bundled into the image.

import type {
  EvidenceRecord,
  GatewayHealth,
  IdResponse,
  IngestRequest,
  MemoryFilter,
  MemoryRecord,
  QueryRequest,
  QueryResult,
  SynthesisRecord,
  SynthesisTriggerRequest,
} from './types';

// Trailing slashes are trimmed so a misconfigured base such as
// `https://gateway.example.com/` does not produce double-slash URLs.
const BASE_URL: string = (
  process.env.NEXT_PUBLIC_GATEWAY_BASE_URL ?? ''
).replace(/\/+$/, '');

const TOKEN_STORAGE_KEY = 'knowledge.ui.token';

export function getToken(): string {
  if (typeof window === 'undefined') return '';
  try {
    return window.localStorage.getItem(TOKEN_STORAGE_KEY) ?? '';
  } catch {
    return '';
  }
}

export function setToken(token: string): void {
  if (typeof window === 'undefined') return;
  try {
    if (token) window.localStorage.setItem(TOKEN_STORAGE_KEY, token);
    else window.localStorage.removeItem(TOKEN_STORAGE_KEY);
  } catch {
    // localStorage unavailable (e.g. private mode) — tokens simply do
    // not persist; requests proceed unauthenticated (dev mode).
  }
}

export function gatewayBaseUrl(): string {
  return BASE_URL;
}

/** Error carrying the HTTP status and any parsed error body. */
export class ApiError extends Error {
  readonly status: number;
  readonly body: unknown;
  constructor(status: number, message: string, body: unknown) {
    super(message);
    this.name = 'ApiError';
    this.status = status;
    this.body = body;
  }
}

interface RequestOptions {
  method?: 'GET' | 'POST' | 'PUT' | 'DELETE';
  body?: unknown;
  query?: Record<string, string | number | boolean | undefined>;
  signal?: AbortSignal;
}

function buildUrl(path: string, query?: RequestOptions['query']): string {
  const url = `${BASE_URL}${path}`;
  if (!query) return url;
  const params = new URLSearchParams();
  for (const [key, value] of Object.entries(query)) {
    if (value !== undefined && value !== '') params.set(key, String(value));
  }
  const qs = params.toString();
  return qs ? `${url}?${qs}` : url;
}

function authHeaders(): Record<string, string> {
  const token = getToken();
  return token ? { Authorization: `Bearer ${token}` } : {};
}

interface ErrorBody {
  message?: string;
}

function isErrorBody(v: unknown): v is ErrorBody {
  return typeof v === 'object' && v !== null && 'message' in v;
}

async function request<T>(path: string, opts: RequestOptions = {}): Promise<T> {
  const headers: Record<string, string> = {
    Accept: 'application/json',
    ...authHeaders(),
  };

  let body: string | undefined;
  if (opts.body !== undefined) {
    headers['Content-Type'] = 'application/json';
    body = JSON.stringify(opts.body);
  }

  const res = await fetch(buildUrl(path, opts.query), {
    method: opts.method ?? 'GET',
    headers,
    body,
    signal: opts.signal,
  });

  const text = await res.text();
  let parsed: unknown = undefined;
  if (text) {
    try {
      parsed = JSON.parse(text);
    } catch {
      parsed = text;
    }
  }

  if (!res.ok) {
    const message =
      (isErrorBody(parsed) && parsed.message) ||
      `${res.status} ${res.statusText}`;
    throw new ApiError(res.status, message, parsed);
  }

  return parsed as T;
}

// The gateway's `writeRaw` (server/internal/gateway/gateway.go) emits a
// literal `null` body for an empty payload, so a list endpoint can resolve
// to `null` rather than `[]`. Normalize at the typed boundary so callers
// can always iterate the result safely.
function asArray<T>(value: T[] | null | undefined): T[] {
  return Array.isArray(value) ? value : [];
}

// ── Evidence / query ────────────────────────────────────────────────

/** `POST /api/v1/ingest` — append a message/document to a scope. */
export function ingest(req: IngestRequest): Promise<IdResponse> {
  return request<IdResponse>('/api/v1/ingest', { method: 'POST', body: req });
}

/** `POST /api/v1/query` — hybrid (FTS + recency + vector) search. */
export function query(
  req: QueryRequest,
  signal?: AbortSignal,
): Promise<QueryResult[]> {
  return request<QueryResult[] | null>('/api/v1/query', {
    method: 'POST',
    body: req,
    signal,
  }).then(asArray);
}

/** `GET /api/v1/evidence/{id}` — fetch a single evidence record. */
export function getEvidence(
  id: string,
  signal?: AbortSignal,
): Promise<EvidenceRecord> {
  return request<EvidenceRecord>(
    `/api/v1/evidence/${encodeURIComponent(id)}`,
    { signal },
  );
}

// ── Memories ────────────────────────────────────────────────────────

/** `GET /api/v1/memories?scope_id=…` — list memory objects for a scope. */
export function listMemories(
  scopeId: string,
  opts: { filter?: MemoryFilter; limit?: number } = {},
  signal?: AbortSignal,
): Promise<MemoryRecord[]> {
  return request<MemoryRecord[] | null>('/api/v1/memories', {
    query: { scope_id: scopeId, filter: opts.filter, limit: opts.limit },
    signal,
  }).then(asArray);
}

/**
 * `POST /api/v1/forget/{scope_id}` — cryptographically forget an entire
 * scope (irreversible DEK destruction). Returns 204.
 */
export function forgetScope(scopeId: string): Promise<void> {
  return request<void>(
    `/api/v1/forget/${encodeURIComponent(scopeId)}`,
    { method: 'POST' },
  );
}

// ── Synthesis ───────────────────────────────────────────────────────

/** `POST /api/v1/synthesis/trigger` — kick off a synthesis run. */
export function triggerSynthesis(
  req: SynthesisTriggerRequest,
): Promise<SynthesisRecord> {
  return request<SynthesisRecord>('/api/v1/synthesis/trigger', {
    method: 'POST',
    body: req,
  });
}

/** `GET /api/v1/synthesis/recent?scope_id=…` — recent runs for a scope. */
export function recentSyntheses(
  scopeId: string,
  signal?: AbortSignal,
): Promise<SynthesisRecord[]> {
  return request<SynthesisRecord[] | null>('/api/v1/synthesis/recent', {
    query: { scope_id: scopeId },
    signal,
  }).then(asArray);
}

/** `GET /api/v1/synthesis/{id}/status` — single status snapshot. */
export function synthesisStatus(
  id: string,
  signal?: AbortSignal,
): Promise<SynthesisRecord> {
  return request<SynthesisRecord>(
    `/api/v1/synthesis/${encodeURIComponent(id)}/status`,
    { signal },
  );
}

// ── Health ──────────────────────────────────────────────────────────

/** `GET /health` — gateway + substrate readiness envelope. */
export function health(signal?: AbortSignal): Promise<GatewayHealth> {
  return request<GatewayHealth>('/health', { signal });
}

// ── SSE: synthesis status streaming ─────────────────────────────────

/** A parsed Server-Sent Event from the synthesis status stream. */
export interface SynthesisStreamEvent {
  event: string;
  data: unknown;
}

export interface StreamHandlers {
  onStatus?: (record: SynthesisRecord) => void;
  onDone?: () => void;
  onError?: (err: Error) => void;
}

/**
 * Subscribe to `GET /api/v1/synthesis/{id}/status?stream=true` as SSE.
 *
 * `EventSource` cannot set an `Authorization` header, so this reads the
 * stream with `fetch` + a `ReadableStream` reader and parses the SSE
 * framing by hand. The gateway emits `event: status` (a SynthesisRecord
 * payload), `event: done` (terminal), and `event: error`, plus comment
 * heartbeats (lines starting with `:`) which are ignored.
 *
 * Returns an abort function; call it to close the stream.
 */
export function streamSynthesisStatus(
  id: string,
  handlers: StreamHandlers,
): () => void {
  const controller = new AbortController();

  void (async () => {
    try {
      const res = await fetch(
        buildUrl(`/api/v1/synthesis/${encodeURIComponent(id)}/status`, {
          stream: true,
        }),
        {
          headers: { Accept: 'text/event-stream', ...authHeaders() },
          signal: controller.signal,
        },
      );
      if (!res.ok || !res.body) {
        throw new ApiError(
          res.status,
          `synthesis stream failed: ${res.status} ${res.statusText}`,
          null,
        );
      }

      const reader = res.body.getReader();
      const decoder = new TextDecoder();
      let buffer = '';
      let sawDoneEvent = false;

      // SSE frames are separated by a blank line; split on \n\n and
      // keep the trailing partial frame in `buffer`.
      for (;;) {
        const { value, done } = await reader.read();
        if (done) break;
        buffer += decoder.decode(value, { stream: true });
        let sep: number;
        while ((sep = buffer.indexOf('\n\n')) !== -1) {
          const frame = buffer.slice(0, sep);
          buffer = buffer.slice(sep + 2);
          if (dispatchFrame(frame, handlers) === 'done') sawDoneEvent = true;
        }
      }
      // The gateway normally emits an explicit `event: done`, which
      // dispatchFrame already forwarded to onDone. Only synthesize a
      // completion here as a fallback for streams that close without
      // one, so onDone fires exactly once.
      if (!sawDoneEvent) handlers.onDone?.();
    } catch (err) {
      if (controller.signal.aborted) return; // caller-initiated close
      handlers.onError?.(err instanceof Error ? err : new Error(String(err)));
    }
  })();

  return () => controller.abort();
}

/** Parse and dispatch one SSE frame; returns the event name handled. */
function dispatchFrame(frame: string, handlers: StreamHandlers): string {
  let event = 'message';
  const dataLines: string[] = [];
  for (const raw of frame.split('\n')) {
    const line = raw.replace(/\r$/, '');
    if (!line || line.startsWith(':')) continue; // heartbeat / comment
    // Per the SSE spec, strip only a single optional leading space after
    // the field colon — not all surrounding whitespace — so payloads keep
    // any significant whitespace if a future event emits non-JSON data.
    if (line.startsWith('event:')) event = line.slice(6).replace(/^ /, '');
    else if (line.startsWith('data:')) dataLines.push(line.slice(5).replace(/^ /, ''));
  }
  if (dataLines.length === 0 && event === 'message') return event;

  let data: unknown = undefined;
  const payload = dataLines.join('\n');
  if (payload) {
    try {
      data = JSON.parse(payload);
    } catch {
      data = payload;
    }
  }

  switch (event) {
    case 'status':
      handlers.onStatus?.(data as SynthesisRecord);
      break;
    case 'done':
      handlers.onDone?.();
      break;
    case 'error':
      handlers.onError?.(
        new Error(
          isErrorBody(data) && data.message
            ? data.message
            : 'synthesis stream error',
        ),
      );
      break;
    default:
      break;
  }
  return event;
}
