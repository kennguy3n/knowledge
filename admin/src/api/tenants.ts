import { request } from './http';
import type {
  CreateTenantRequest,
  Member,
  Tenant,
} from './types';

const BASE = '/api/v1/tenants';

/** `GET /api/v1/tenants` — list tenants. */
export function listTenants(signal?: AbortSignal): Promise<Tenant[]> {
  return request<Tenant[]>(BASE, { signal });
}

/** `GET /api/v1/tenants/{id}` — load a single tenant. */
export function getTenant(id: string, signal?: AbortSignal): Promise<Tenant> {
  return request<Tenant>(`${BASE}/${encodeURIComponent(id)}`, { signal });
}

/** `POST /api/v1/tenants` — provision a new tenant. */
export function createTenant(body: CreateTenantRequest): Promise<Tenant> {
  return request<Tenant>(BASE, { method: 'POST', body });
}

/** `DELETE /api/v1/tenants/{id}` — delete a tenant. */
export function deleteTenant(id: string): Promise<void> {
  return request<void>(`${BASE}/${encodeURIComponent(id)}`, {
    method: 'DELETE',
  });
}

/** `POST /api/v1/tenants/{id}/key/rotate` — rotate the tenant keypair. */
export function rotateTenantKey(id: string): Promise<Tenant> {
  return request<Tenant>(`${BASE}/${encodeURIComponent(id)}/key/rotate`, {
    method: 'POST',
  });
}

/** `GET /api/v1/tenants/{id}/members` — list tenant members. */
export function listMembers(
  id: string,
  signal?: AbortSignal,
): Promise<Member[]> {
  return request<Member[]>(`${BASE}/${encodeURIComponent(id)}/members`, {
    signal,
  });
}
