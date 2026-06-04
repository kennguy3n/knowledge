import { useState } from 'react';
import { connectorsApi } from '../api';
import type { ConnectorKind, ConnectorStatus } from '../api';
import { useAsync } from '../hooks/useAsync';
import {
  Card,
  ErrorBanner,
  Notice,
  PageHeader,
  Spinner,
  StatusBadge,
} from '../components/ui';

const KINDS: ConnectorKind[] = [
  'google_drive',
  'one_drive',
  'notion',
  'jira',
  'confluence',
  'git_hub',
  'slack',
  'figma',
  'hub_spot',
  'email',
  'generic_webhook',
];

export default function Connectors() {
  const list = useAsync((signal) => connectorsApi.listConnectors(signal), []);
  const [actionError, setActionError] = useState<Error | undefined>();
  const [busy, setBusy] = useState<string | undefined>();

  // Create form.
  const [kind, setKind] = useState<string>('google_drive');
  const [scopeId, setScopeId] = useState('');
  const [configJson, setConfigJson] = useState('{}');

  async function run(key: string, fn: () => Promise<unknown>) {
    setBusy(key);
    setActionError(undefined);
    try {
      await fn();
      list.reload();
    } catch (err) {
      setActionError(err instanceof Error ? err : new Error(String(err)));
    } finally {
      setBusy(undefined);
    }
  }

  // Re-authenticate a connector: the OAuth start endpoint is an
  // authenticated JSON call (it needs the bearer token and the provider
  // client_id / redirect_uri), so we can't just window.open its URL.
  // Open the tab synchronously inside the click gesture (so it isn't
  // popup-blocked), fetch the provider authorize_url through the client,
  // then point the tab at it.
  function onReauth(c: ConnectorStatus) {
    const clientId = window
      .prompt(
        `OAuth client_id for the ${c.kind} provider app (used to build the authorization URL):`,
      )
      ?.trim();
    if (!clientId) return;
    const popup = window.open('about:blank', '_blank');
    if (popup) popup.opener = null;
    void run(`reauth-${c.instanceId}`, async () => {
      try {
        const res = await connectorsApi.startOAuth(c.instanceId, {
          client_id: clientId,
          redirect_uri: connectorsApi.oauthCallbackUrl(),
        });
        if (popup) popup.location.href = res.authorize_url;
        else window.open(res.authorize_url, '_blank', 'noopener');
      } catch (err) {
        popup?.close();
        throw err;
      }
    });
  }

  function onCreate(e: React.FormEvent) {
    e.preventDefault();
    void run('create', () =>
      connectorsApi.createConnector({
        kind,
        scope_id: scopeId.trim(),
        config_json: configJson.trim() || '{}',
      }),
    );
  }

  return (
    <div>
      <PageHeader
        title="Connectors"
        description="Source-system connector instances: create, sync, re-auth, and remove."
        actions={
          <button className="btn-sm" onClick={() => list.reload()}>
            Refresh
          </button>
        }
      />

      <ErrorBanner error={list.error ?? actionError} />

      <Card title="Create connector">
        <form onSubmit={onCreate}>
          <div className="row">
            <div className="field">
              <label>Kind</label>
              <select value={kind} onChange={(e) => setKind(e.target.value)}>
                {KINDS.map((k) => (
                  <option key={k} value={k}>
                    {k}
                  </option>
                ))}
              </select>
            </div>
            <div className="field">
              <label>Scope ID (UUID)</label>
              <input
                value={scopeId}
                onChange={(e) => setScopeId(e.target.value)}
                placeholder="00000000-0000-0000-0000-000000000000"
                required
              />
            </div>
          </div>
          <div className="field">
            <label>Config JSON</label>
            <textarea
              value={configJson}
              onChange={(e) => setConfigJson(e.target.value)}
              rows={3}
            />
          </div>
          <button className="btn-primary" type="submit" disabled={busy === 'create'}>
            {busy === 'create' ? 'Creating…' : 'Create connector'}
          </button>
        </form>
      </Card>

      <Card title="Connector instances">
        {list.loading && <Spinner />}
        {!list.loading && (list.data?.length ?? 0) === 0 && (
          <Notice>No connector instances registered.</Notice>
        )}
        {(list.data?.length ?? 0) > 0 && (
          <table>
            <thead>
              <tr>
                <th>Instance</th>
                <th>Kind</th>
                <th>Scope</th>
                <th>Mode</th>
                <th>Status</th>
                <th>Last synced</th>
                <th>Actions</th>
              </tr>
            </thead>
            <tbody>
              {list.data!.map((c) => (
                <ConnectorRow
                  key={c.instanceId}
                  c={c}
                  busy={busy}
                  onSync={() =>
                    run(`sync-${c.instanceId}`, () =>
                      connectorsApi.syncConnector(c.instanceId),
                    )
                  }
                  onReauth={() => onReauth(c)}
                  onDelete={() => {
                    if (
                      window.confirm(
                        `Delete connector ${c.instanceId}? This cannot be undone.`,
                      )
                    ) {
                      void run(`delete-${c.instanceId}`, () =>
                        connectorsApi.deleteConnector(c.instanceId),
                      );
                    }
                  }}
                />
              ))}
            </tbody>
          </table>
        )}
      </Card>
    </div>
  );
}

function ConnectorRow({
  c,
  busy,
  onSync,
  onReauth,
  onDelete,
}: {
  c: ConnectorStatus;
  busy: string | undefined;
  onSync: () => void;
  onReauth: () => void;
  onDelete: () => void;
}) {
  return (
    <tr>
      <td className="mono">{c.instanceId}</td>
      <td>{c.kind}</td>
      <td className="mono">{c.scopeId}</td>
      <td>{c.syncMode}</td>
      <td>
        <StatusBadge status={c.syncStatus} />
        {c.lastError && <div className="muted">{c.lastError}</div>}
      </td>
      <td className="muted">
        {c.lastSyncedAt
          ? new Date(c.lastSyncedAt * 1000).toLocaleString()
          : '—'}
      </td>
      <td>
        <div className="row" style={{ gap: 6 }}>
          <button
            className="btn-sm row-fixed"
            onClick={onSync}
            disabled={busy === `sync-${c.instanceId}`}
          >
            Sync
          </button>
          <button className="btn-sm row-fixed" onClick={onReauth}>
            Re-auth
          </button>
          <button
            className="btn-sm btn-danger row-fixed"
            onClick={onDelete}
            disabled={busy === `delete-${c.instanceId}`}
          >
            Delete
          </button>
        </div>
      </td>
    </tr>
  );
}
