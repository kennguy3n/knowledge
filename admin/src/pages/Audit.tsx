import { useState } from 'react';
import { auditApi } from '../api';
import type { AuditEvent, AuditQuery } from '../api';
import {
  Card,
  ErrorBanner,
  JsonBlock,
  Notice,
  PageHeader,
  Spinner,
} from '../components/ui';

export default function Audit() {
  const [q, setQ] = useState<AuditQuery>({ limit: 100 });
  const [rows, setRows] = useState<AuditEvent[] | undefined>();
  const [error, setError] = useState<Error | undefined>();
  const [loading, setLoading] = useState(false);
  const [detail, setDetail] = useState<unknown>();

  function set<K extends keyof AuditQuery>(key: K, value: AuditQuery[K]) {
    setQ((prev) => ({ ...prev, [key]: value }));
  }

  async function onQuery(e: React.FormEvent) {
    e.preventDefault();
    setLoading(true);
    setError(undefined);
    setDetail(undefined);
    try {
      const clean: AuditQuery = {
        tenant_id: q.tenant_id?.trim() || undefined,
        scope_id: q.scope_id?.trim() || undefined,
        action: q.action?.trim() || undefined,
        actor: q.actor?.trim() || undefined,
        from: q.from || undefined,
        to: q.to || undefined,
        limit: q.limit,
      };
      setRows(await auditApi.queryAudit(clean));
    } catch (err) {
      setError(err instanceof Error ? err : new Error(String(err)));
    } finally {
      setLoading(false);
    }
  }

  return (
    <div>
      <PageHeader
        title="Audit log"
        description="Query the tamper-evident audit event log by tenant, scope, action, or actor."
      />

      <ErrorBanner error={error} />

      <Card title="Filter">
        <form onSubmit={onQuery}>
          <div className="row">
            <div className="field">
              <label>Tenant ID</label>
              <input
                value={q.tenant_id ?? ''}
                onChange={(e) => set('tenant_id', e.target.value)}
              />
            </div>
            <div className="field">
              <label>Scope ID</label>
              <input
                value={q.scope_id ?? ''}
                onChange={(e) => set('scope_id', e.target.value)}
              />
            </div>
            <div className="field">
              <label>Action</label>
              <input
                value={q.action ?? ''}
                onChange={(e) => set('action', e.target.value)}
              />
            </div>
            <div className="field">
              <label>Actor</label>
              <input
                value={q.actor ?? ''}
                onChange={(e) => set('actor', e.target.value)}
              />
            </div>
          </div>
          <div className="row">
            <div className="field">
              <label>From (RFC3339)</label>
              <input
                value={q.from ?? ''}
                onChange={(e) => set('from', e.target.value)}
                placeholder="2026-01-01T00:00:00Z"
              />
            </div>
            <div className="field">
              <label>To (RFC3339)</label>
              <input
                value={q.to ?? ''}
                onChange={(e) => set('to', e.target.value)}
                placeholder="2026-12-31T23:59:59Z"
              />
            </div>
            <div className="field">
              <label>Limit</label>
              <input
                type="number"
                min={1}
                value={q.limit ?? 100}
                onChange={(e) => set('limit', Number(e.target.value))}
              />
            </div>
            <div className="field row-fixed">
              <label>&nbsp;</label>
              <button className="btn-primary" type="submit" disabled={loading}>
                {loading ? 'Querying…' : 'Query'}
              </button>
            </div>
          </div>
        </form>
      </Card>

      <Card title="Events">
        {loading && <Spinner />}
        {!loading && rows && rows.length === 0 && (
          <Notice>No audit events match this query.</Notice>
        )}
        {rows && rows.length > 0 && (
          <table>
            <thead>
              <tr>
                <th>Time</th>
                <th>Action</th>
                <th>Actor</th>
                <th>Tenant</th>
                <th>Scope</th>
                <th></th>
              </tr>
            </thead>
            <tbody>
              {rows.map((ev) => (
                <tr key={ev.id}>
                  <td className="muted">
                    {new Date(ev.created_at).toLocaleString()}
                  </td>
                  <td>{ev.action}</td>
                  <td className="mono">{ev.actor}</td>
                  <td className="mono">{ev.tenant_id}</td>
                  <td className="mono">{ev.scope_id || '—'}</td>
                  <td>
                    {ev.detail != null && (
                      <button
                        className="btn-sm"
                        onClick={() => setDetail(ev.detail)}
                      >
                        Detail
                      </button>
                    )}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </Card>

      {detail != null && (
        <Card title="Event detail">
          <JsonBlock value={detail} />
        </Card>
      )}
    </div>
  );
}
