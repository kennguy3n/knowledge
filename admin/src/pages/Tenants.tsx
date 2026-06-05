import { useState } from 'react';
import { tenantsApi } from '../api';
import type { Member, SynthesisTier, Tenant } from '../api';
import { useAsync } from '../hooks/useAsync';
import {
  Card,
  ErrorBanner,
  Notice,
  PageHeader,
  Spinner,
  StatusBadge,
} from '../components/ui';

const TIERS: SynthesisTier[] = ['basic', 'standard', 'premium'];

export default function Tenants() {
  const list = useAsync((signal) => tenantsApi.listTenants(signal), []);
  const [actionError, setActionError] = useState<Error | undefined>();
  // Track every in-flight action by key so concurrent operations on
  // different rows each keep their own button disabled (a single string
  // key would let a later action re-enable an earlier in-flight one).
  const [busy, setBusy] = useState<ReadonlySet<string>>(() => new Set());
  const [selected, setSelected] = useState<string | undefined>();

  const [name, setName] = useState('');
  const [tier, setTier] = useState<SynthesisTier>('standard');
  const [connectorLimit, setConnectorLimit] = useState(10);
  const [retentionDays, setRetentionDays] = useState(365);

  async function run(key: string, fn: () => Promise<unknown>): Promise<boolean> {
    setBusy((prev) => new Set(prev).add(key));
    setActionError(undefined);
    try {
      await fn();
      list.reload();
      return true;
    } catch (err) {
      setActionError(err instanceof Error ? err : new Error(String(err)));
      return false;
    } finally {
      setBusy((prev) => {
        const next = new Set(prev);
        next.delete(key);
        return next;
      });
    }
  }

  function onCreate(e: React.FormEvent) {
    e.preventDefault();
    void run('create', () =>
      tenantsApi.createTenant({
        name: name.trim(),
        config: {
          synthesis_tier: tier,
          connector_limit: connectorLimit,
          retention_days: retentionDays,
        },
      }),
    ).then((ok) => {
      // Only clear the form once the tenant is actually created, so a
      // failed request preserves the operator's input.
      if (ok) setName('');
    });
  }

  return (
    <div>
      <PageHeader
        title="Tenants"
        description="B2B accounts: provision, inspect configuration, rotate keys, and manage members."
        actions={
          <button className="btn-sm" onClick={() => list.reload()}>
            Refresh
          </button>
        }
      />

      <ErrorBanner error={list.error} />
      <ErrorBanner error={actionError} />

      <Card title="Create tenant">
        <form onSubmit={onCreate}>
          <div className="row">
            <div className="field">
              <label>Name</label>
              <input
                value={name}
                onChange={(e) => setName(e.target.value)}
                placeholder="Acme Corp"
                required
              />
            </div>
            <div className="field">
              <label>Synthesis tier</label>
              <select
                value={tier}
                onChange={(e) => setTier(e.target.value as SynthesisTier)}
              >
                {TIERS.map((t) => (
                  <option key={t} value={t}>
                    {t}
                  </option>
                ))}
              </select>
            </div>
            <div className="field">
              <label>Connector limit</label>
              <input
                type="number"
                min={0}
                value={connectorLimit}
                onChange={(e) => setConnectorLimit(Number(e.target.value))}
              />
            </div>
            <div className="field">
              <label>Retention (days)</label>
              <input
                type="number"
                min={0}
                value={retentionDays}
                onChange={(e) => setRetentionDays(Number(e.target.value))}
              />
            </div>
          </div>
          <button
            className="btn-primary"
            type="submit"
            disabled={busy.has('create')}
          >
            {busy.has('create') ? 'Creating…' : 'Create tenant'}
          </button>
        </form>
      </Card>

      <Card title="Tenants">
        {list.loading && <Spinner />}
        {!list.loading && (list.data?.length ?? 0) === 0 && (
          <Notice>No tenants provisioned.</Notice>
        )}
        {(list.data?.length ?? 0) > 0 && (
          <table>
            <thead>
              <tr>
                <th>ID</th>
                <th>Name</th>
                <th>Tier</th>
                <th>Limits</th>
                <th>Key algo</th>
                <th>Created</th>
                <th>Actions</th>
              </tr>
            </thead>
            <tbody>
              {list.data!.map((t) => (
                <TenantRow
                  key={t.id}
                  t={t}
                  busy={busy}
                  selected={selected === t.id}
                  onToggle={() =>
                    setSelected(selected === t.id ? undefined : t.id)
                  }
                  onRotate={() =>
                    run(`rotate-${t.id}`, () => tenantsApi.rotateTenantKey(t.id))
                  }
                  onDelete={() => {
                    if (window.confirm(`Delete tenant ${t.name} (${t.id})?`)) {
                      void run(`delete-${t.id}`, () =>
                        tenantsApi.deleteTenant(t.id),
                      );
                    }
                  }}
                />
              ))}
            </tbody>
          </table>
        )}
      </Card>

      {selected && <MembersCard tenantId={selected} />}
    </div>
  );
}

function TenantRow({
  t,
  busy,
  selected,
  onToggle,
  onRotate,
  onDelete,
}: {
  t: Tenant;
  busy: ReadonlySet<string>;
  selected: boolean;
  onToggle: () => void;
  onRotate: () => void;
  onDelete: () => void;
}) {
  return (
    <tr>
      <td className="mono">{t.id}</td>
      <td>{t.name}</td>
      <td>{t.config.synthesis_tier}</td>
      <td className="muted">
        {t.config.connector_limit} conn · {t.config.retention_days}d
      </td>
      <td className="mono">{t.key.algorithm}</td>
      <td className="muted">{new Date(t.created_at).toLocaleDateString()}</td>
      <td>
        <div className="row" style={{ gap: 6 }}>
          <button className="btn-sm row-fixed" onClick={onToggle}>
            {selected ? 'Hide members' : 'Members'}
          </button>
          <button
            className="btn-sm row-fixed"
            onClick={onRotate}
            disabled={busy.has(`rotate-${t.id}`)}
          >
            Rotate key
          </button>
          <button
            className="btn-sm btn-danger row-fixed"
            onClick={onDelete}
            disabled={busy.has(`delete-${t.id}`)}
          >
            Delete
          </button>
        </div>
      </td>
    </tr>
  );
}

function MembersCard({ tenantId }: { tenantId: string }) {
  const members = useAsync(
    (signal) => tenantsApi.listMembers(tenantId, signal),
    [tenantId],
  );
  return (
    <Card title={`Members · ${tenantId}`}>
      <ErrorBanner error={members.error} />
      {members.loading && <Spinner />}
      {!members.loading && (members.data?.length ?? 0) === 0 && (
        <Notice>No members in this tenant.</Notice>
      )}
      {!members.loading && (members.data?.length ?? 0) > 0 && (
        <table>
          <thead>
            <tr>
              <th>User ID</th>
              <th>Email</th>
              <th>Status</th>
              <th>Updated</th>
            </tr>
          </thead>
          <tbody>
            {members.data!.map((m: Member) => (
              <tr key={m.user_id}>
                <td className="mono">{m.user_id}</td>
                <td>{m.email}</td>
                <td>
                  <StatusBadge status={m.status} />
                </td>
                <td className="muted">
                  {new Date(m.updated_at).toLocaleString()}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </Card>
  );
}
