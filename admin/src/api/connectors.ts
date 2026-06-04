import { gatewayBaseUrl, request } from './http';
import type {
  AuthenticateConnectorRequest,
  ConnectorStatus,
  CreateConnectorRequest,
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
 * Begin the OAuth authorization flow for a connector. The gateway
 * exposes `GET /api/v1/connectors/{id}/oauth/start` which redirects to
 * the provider; the admin opens it in a new tab. Prefixed with
 * `gatewayBaseUrl()` so cross-origin (`VITE_GATEWAY_BASE_URL`)
 * deployments open the gateway, not the SPA's own origin.
 */
export function oauthStartUrl(id: string): string {
  return `${gatewayBaseUrl()}/api/v1/connectors/${encodeURIComponent(id)}/oauth/start`;
}
