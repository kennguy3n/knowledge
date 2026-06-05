import { useState } from 'react';
import { synthesisApi } from '../api';
import type { SynthesisRecord } from '../api';
import {
  Card,
  ErrorBanner,
  JsonBlock,
  Notice,
  PageHeader,
  Spinner,
} from '../components/ui';

export default function Synthesis() {
  const [scopeId, setScopeId] = useState('');
  const [trigger, setTrigger] = useState('ManualUserAction');
  const [recent, setRecent] = useState<SynthesisRecord[] | undefined>();
  const [error, setError] = useState<Error | undefined>();
  const [loading, setLoading] = useState(false);
  const [lastTrigger, setLastTrigger] = useState<SynthesisRecord | undefined>();

  const [statusId, setStatusId] = useState('');
  const [status, setStatus] = useState<SynthesisRecord | undefined>();

  async function loadRecent(e?: React.FormEvent) {
    e?.preventDefault();
    setLoading(true);
    setError(undefined);
    try {
      setRecent(await synthesisApi.recentSyntheses(scopeId.trim()));
    } catch (err) {
      setError(err instanceof Error ? err : new Error(String(err)));
    } finally {
      setLoading(false);
    }
  }

  async function onTrigger() {
    setError(undefined);
    try {
      const rec = await synthesisApi.triggerSynthesis({
        scope_id: scopeId.trim(),
        trigger: trigger.trim() || undefined,
      });
      setLastTrigger(rec);
      void loadRecent();
    } catch (err) {
      setError(err instanceof Error ? err : new Error(String(err)));
    }
  }

  async function onCheckStatus(e: React.FormEvent) {
    e.preventDefault();
    setError(undefined);
    try {
      setStatus(await synthesisApi.synthesisStatus(statusId.trim()));
    } catch (err) {
      setError(err instanceof Error ? err : new Error(String(err)));
    }
  }

  return (
    <div>
      <PageHeader
        title="Synthesis"
        description="Trigger synthesis runs for a scope and inspect recent run status."
      />

      <ErrorBanner error={error} />

      <Card title="Trigger / recent runs">
        <form onSubmit={loadRecent}>
          <div className="row">
            <div className="field">
              <label>Scope ID (UUID)</label>
              <input
                value={scopeId}
                onChange={(e) => setScopeId(e.target.value)}
                placeholder="00000000-0000-0000-0000-000000000000"
                required
              />
            </div>
            <div className="field">
              <label>Trigger</label>
              <input
                value={trigger}
                onChange={(e) => setTrigger(e.target.value)}
              />
            </div>
            <div className="field row-fixed">
              <label>&nbsp;</label>
              <button type="submit" disabled={loading || !scopeId.trim()}>
                Load recent
              </button>
            </div>
            <div className="field row-fixed">
              <label>&nbsp;</label>
              <button
                type="button"
                className="btn-primary"
                onClick={onTrigger}
                disabled={!scopeId.trim()}
              >
                Trigger synthesis
              </button>
            </div>
          </div>
        </form>

        {lastTrigger && (
          <>
            <p className="muted">Trigger accepted:</p>
            <JsonBlock value={lastTrigger} />
          </>
        )}

        {loading && <Spinner />}
        {!loading && recent && recent.length === 0 && (
          <Notice>No recent synthesis runs for this scope.</Notice>
        )}
        {recent && recent.length > 0 && <RecentTable rows={recent} />}
      </Card>

      <Card title="Run status">
        <form onSubmit={onCheckStatus}>
          <div className="row">
            <div className="field">
              <label>Synthesis ID (UUID)</label>
              <input
                value={statusId}
                onChange={(e) => setStatusId(e.target.value)}
                placeholder="synthesis run id"
                required
              />
            </div>
            <div className="field row-fixed">
              <label>&nbsp;</label>
              <button type="submit" disabled={!statusId.trim()}>
                Check status
              </button>
            </div>
          </div>
        </form>
        {status && <JsonBlock value={status} />}
      </Card>
    </div>
  );
}

function RecentTable({ rows }: { rows: SynthesisRecord[] }) {
  return (
    <table>
      <thead>
        <tr>
          <th>ID</th>
          <th>Status</th>
          <th>Trigger</th>
          <th>Created</th>
        </tr>
      </thead>
      <tbody>
        {rows.map((r, i) => (
          <tr key={(r.id as string) ?? i}>
            <td className="mono">{str(r.id) ?? '—'}</td>
            <td>{str(r.status) ?? '—'}</td>
            <td>{str(r.trigger) ?? '—'}</td>
            <td className="muted">{str(r.created_at) ?? '—'}</td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}

function str(v: unknown): string | undefined {
  return typeof v === 'string' ? v : undefined;
}
