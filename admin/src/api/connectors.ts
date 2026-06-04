import { gatewayBaseUrl, request } from './http';
import type {
  AuthenticateConnectorRequest,
  ConnectorStatus,
  CreateConnectorRequest,
  OAuthStartParams,
  OAuthStartResponse,
} from './types';

const BASE = '/api/v1/connectors';

/** `GET /api/v1/connectors` — list all connector instances. */
export function listConnectors(signal?: AbortSignal): Promise<ConnectorStatus[]> {
  return request<ConnectorStatus[]>(BASE, { signal });
}

/** `POST /api/v1/connectors` — register a connector instance. */
export function createConnector(
  body: CreateConnectorRequest,
): Promise<unknown> {
  return request<unknown>(BASE, { method: 'POST', body });
}

/** `POST /api/v1/connectors/{id}/authenticate` — complete OAuth re-auth. */
export function authenticateConnector(
  id: string,
  body: AuthenticateConnectorRequest,
): Promise<unknown> {
  return request<unknown>(`${BASE}/${encodeURIComponent(id)}/authenticate`, {
    method: 'POST',
    body,
  });
}

/** `POST /api/v1/connectors/{id}/sync` — trigger an incremental sync. */
export function syncConnector(id: string): Promise<unknown> {
  return request<unknown>(`${BASE}/${encodeURIComponent(id)}/sync`, {
    method: 'POST',
  });
}

/** `GET /api/v1/connectors/{id}/status` — current status of one connector. */
export function connectorStatus(
  id: string,
  signal?: AbortSignal,
): Promise<ConnectorStatus> {
  return request<ConnectorStatus>(
    `${BASE}/${encodeURIComponent(id)}/status`,
    { signal },
  );
}

/** `DELETE /api/v1/connectors/{id}` — remove a connector instance. */
export function deleteConnector(id: string): Promise<void> {
  return request<void>(`${BASE}/${encodeURIComponent(id)}`, {
    method: 'DELETE',
  });
}

/**
 * `GET /api/v1/connectors/{id}/oauth/start` — begin the OAuth flow.
 *
 * This is an authenticated JSON endpoint (not a redirect): it requires
 * `client_id`/`redirect_uri` query params and returns the provider
 * `authorize_url` plus a CSRF `state`. It must be called through the
 * authenticated client so it carries the bearer token (a bare
 * `window.open` of this path 401s when the gateway has auth enabled, and
 * would only ever receive JSON rather than a redirect). The caller then
 * navigates the browser to `authorize_url` — the provider's own origin,
 * which needs no gateway token.
 */
export function startOAuth(
  id: string,
  params: OAuthStartParams,
  signal?: AbortSignal,
): Promise<OAuthStartResponse> {
  return request<OAuthStartResponse>(
    `${BASE}/${encodeURIComponent(id)}/oauth/start`,
    {
      query: { client_id: params.client_id, redirect_uri: params.redirect_uri },
      signal,
    },
  );
}

/**
 * Absolute URL of the gateway's OAuth callback
 * (`GET /api/v1/connectors/oauth/callback`), used as the default
 * `redirect_uri`. Honours `VITE_GATEWAY_BASE_URL`; falls back to the
 * SPA's own origin in same-origin deployments where the base is empty
 * (the nginx image reverse-proxies the gateway on that origin).
 */
export function oauthCallbackUrl(): string {
  const base = gatewayBaseUrl() || window.location.origin;
  return `${base}/api/v1/connectors/oauth/callback`;
}
