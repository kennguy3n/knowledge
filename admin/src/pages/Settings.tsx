import { useState } from 'react';
import { gatewayBaseUrl, getToken, setToken } from '../api';
import { memoriesApi } from '../api';
import {
  Card,
  ErrorBanner,
  Notice,
  PageHeader,
} from '../components/ui';

export default function Settings() {
  const [token, setTokenState] = useState(getToken());
  const [saved, setSaved] = useState(false);

  const [forgetScope, setForgetScope] = useState('');
  const [forgetError, setForgetError] = useState<Error | undefined>();
  const [forgetDone, setForgetDone] = useState(false);
  const [forgetBusy, setForgetBusy] = useState(false);

  function onSaveToken(e: React.FormEvent) {
    e.preventDefault();
    setToken(token.trim());
    setSaved(true);
    setTimeout(() => setSaved(false), 2000);
  }

  async function onForget() {
    if (
      !window.confirm(
        `Cryptographically forget scope ${forgetScope}? This destroys the scope's encryption key and is IRREVERSIBLE.`,
      )
    ) {
      return;
    }
    setForgetBusy(true);
    setForgetError(undefined);
    setForgetDone(false);
    try {
      await memoriesApi.forgetScope(forgetScope.trim());
      setForgetDone(true);
      setForgetScope('');
    } catch (err) {
      setForgetError(err instanceof Error ? err : new Error(String(err)));
    } finally {
      setForgetBusy(false);
    }
  }

  const base = gatewayBaseUrl() || '(same origin)';

  return (
    <div>
      <PageHeader
        title="Settings"
        description="Configure the admin panel's connection to the gateway."
      />

      <Card title="Gateway connection">
        <div className="field">
          <label>Gateway base URL</label>
          <input value={base} readOnly />
          <p className="muted">
            Set at build time via <code>VITE_GATEWAY_BASE_URL</code>. In dev,
            Vite proxies <code>/api</code>, <code>/health</code>, and{' '}
            <code>/metrics</code> to <code>KNOWLEDGE_GATEWAY_URL</code> (default{' '}
            <code>http://localhost:8080</code>).
          </p>
        </div>
      </Card>

      <Card title="Authentication">
        <form onSubmit={onSaveToken}>
          <div className="field">
            <label>Bearer token (API key or tenant JWT)</label>
            <input
              type="password"
              value={token}
              onChange={(e) => setTokenState(e.target.value)}
              placeholder="leave blank for dev-mode (no auth)"
            />
            <p className="muted">
              Stored in this browser&apos;s <code>localStorage</code> and sent as{' '}
              <code>Authorization: Bearer …</code>. The gateway treats an empty
              token as dev-mode when no API key / JWT secret is configured
              server-side.
            </p>
          </div>
          <div className="row" style={{ alignItems: 'center' }}>
            <button className="btn-primary row-fixed" type="submit">
              Save token
            </button>
            {saved && <span className="muted row-fixed">Saved.</span>}
          </div>
        </form>
      </Card>

      <Card title="Danger zone — cryptographic forgetting">
        <ErrorBanner error={forgetError} />
        {forgetDone && (
          <Notice>Scope forgotten. Its encryption key has been destroyed.</Notice>
        )}
        <div className="field">
          <label>Scope ID (UUID)</label>
          <input
            value={forgetScope}
            onChange={(e) => setForgetScope(e.target.value)}
            placeholder="00000000-0000-0000-0000-000000000000"
          />
          <p className="muted">
            Calls <code>POST /api/v1/forget/&#123;scope_id&#125;</code>,
            irreversibly destroying the scope&apos;s data-encryption key.
          </p>
        </div>
        <button
          className="btn-danger"
          onClick={onForget}
          disabled={forgetBusy || !forgetScope.trim()}
        >
          {forgetBusy ? 'Forgetting…' : 'Forget scope'}
        </button>
      </Card>
    </div>
  );
}
