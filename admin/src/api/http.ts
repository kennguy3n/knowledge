// Minimal typed fetch wrapper shared by every domain client.
//
// Base URL resolution:
//   - In dev, Vite proxies `/api`, `/health`, `/metrics` to the
//     gateway (see vite.config.ts), so the default base is "" (same
//     origin). In the nginx production image the SPA is served on the
//     same origin as a reverse-proxied gateway, so "" works there too.
//   - Override at build time with VITE_GATEWAY_BASE_URL for setups
//     where the gateway is on a different origin (CORS must then be
//     enabled gateway-side via KNOWLEDGE_CORS_ORIGINS).
//
// Auth: the gateway accepts a static API key or a tenant JWT as a
// Bearer token (server/internal/middleware/middleware.go). The token
// is read from localStorage so an operator can paste it in via the
// Settings page; it is never bundled into the image.

// Trailing slashes are trimmed so a misconfigured base such as
// `https://gateway.example.com/` does not produce double-slash URLs
// (`…com//api/v1/…`) when concatenated with the leading-slash paths.
const BASE_URL: string = (
  (import.meta.env.VITE_GATEWAY_BASE_URL as string | undefined) ?? ''
).replace(/\/+$/, '');

const TOKEN_STORAGE_KEY = 'knowledge.admin.token';

export function getToken(): string {
  try {
    return localStorage.getItem(TOKEN_STORAGE_KEY) ?? '';
  } catch {
    return '';
  }
}

export function setToken(token: string): void {
  try {
    if (token) localStorage.setItem(TOKEN_STORAGE_KEY, token);
    else localStorage.removeItem(TOKEN_STORAGE_KEY);
  } catch {
    // localStorage unavailable (e.g. private mode) — tokens simply
    // do not persist; requests proceed unauthenticated (dev mode).
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

/** Authorization header for the stored bearer token, if any. */
function authHeaders(): Record<string, string> {
  const token = getToken();
  return token ? { Authorization: `Bearer ${token}` } : {};
}

/**
 * Fetch a `text/plain` resource (e.g. the Prometheus metrics exposition)
 * through the same base-URL and auth handling as {@link request}. The
 * JSON `request<T>` helper cannot be reused because it parses the body;
 * this shares the URL building and bearer-token logic so cross-origin
 * (`VITE_GATEWAY_BASE_URL`) and authenticated deployments work.
 */
export async function requestText(
  path: string,
  opts: Pick<RequestOptions, 'query' | 'signal'> = {},
): Promise<string> {
  const res = await fetch(buildUrl(path, opts.query), {
    headers: { Accept: 'text/plain', ...authHeaders() },
    signal: opts.signal,
  });
  const text = await res.text();
  if (!res.ok) {
    throw new ApiError(res.status, `${res.status} ${res.statusText}`, text);
  }
  return text;
}

export async function request<T>(
  path: string,
  opts: RequestOptions = {},
): Promise<T> {
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

interface ErrorBody {
  message?: string;
}

function isErrorBody(v: unknown): v is ErrorBody {
  return typeof v === 'object' && v !== null && 'message' in v;
}
