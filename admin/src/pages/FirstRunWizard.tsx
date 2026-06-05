import { useState } from 'react';
import { useNavigate } from 'react-router-dom';
import { connectorsApi } from '../api';
import type { ConnectorKind } from '../api';
import {
  WIZARD_CONNECTOR_KINDS,
  connectorLabel,
} from '../lib/connectorKinds';
import { markFirstRunDismissed } from '../lib/firstRun';
import { Card, ErrorBanner, PageHeader } from '../components/ui';

type Step = 1 | 2 | 3 | 4;

/**
 * Four-step onboarding flow for a fresh deployment:
 *   1. Welcome / what this does.
 *   2. Pick one of the ten popular connector kinds.
 *   3. Register the connector and kick off its OAuth2 authorization.
 *   4. Confirmation — the first sync is scheduled; head to the Dashboard.
 *
 * Auto-opened from `App` on first visit when no connectors exist, and
 * reachable any time from the Dashboard "Getting started" card.
 */
export default function FirstRunWizard() {
  const navigate = useNavigate();
  const [step, setStep] = useState<Step>(1);
  const [kind, setKind] = useState<ConnectorKind | null>(null);
  // A fresh scope UUID is suggested so an operator who just wants to
  // get going does not have to know what a scope is; it stays editable
  // for those wiring the connector into an existing scope.
  const [scopeId, setScopeId] = useState<string>(() => freshUuid());
  const [clientId, setClientId] = useState('');
  const [instanceId, setInstanceId] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<Error | undefined>();

  function finish() {
    markFirstRunDismissed();
    navigate('/dashboard');
  }

  function skip() {
    markFirstRunDismissed();
    navigate('/dashboard');
  }

  function pickKind(k: ConnectorKind) {
    setKind(k);
    // Drop any connector created for a previously-picked kind: the
    // instanceId reuse below is only meant to retry OAuth for the *same*
    // kind, so a fresh selection must start a new connector rather than
    // authorizing the stale instance under the wrong kind.
    setInstanceId(null);
    setError(undefined);
    setStep(3);
  }

  // Register the connector, then start its OAuth flow. The provider
  // authorization page is opened in a popup created synchronously
  // inside the click gesture (so it is not popup-blocked) and then
  // pointed at the authorize_url — mirroring the Connectors page
  // re-auth flow.
  //
  // Registration and OAuth-start are two requests. If the first
  // succeeds but the second fails (e.g. a mistyped client id), the
  // connector is already persisted — so on retry we reuse that
  // instance and re-attempt only the OAuth start, rather than creating
  // a duplicate connector each time.
  function connect(e: React.FormEvent) {
    e.preventDefault();
    if (!kind || busy) return;
    const scope = scopeId.trim();
    const client = clientId.trim();
    if (!scope || !client) return;

    const popup = window.open('about:blank', '_blank');
    if (popup) popup.opener = null;

    setBusy(true);
    setError(undefined);
    void (async () => {
      try {
        let id = instanceId;
        if (!id) {
          const created = await connectorsApi.createConnector({
            kind,
            scope_id: scope,
          });
          id = created.instance_id;
          setInstanceId(id);
        }
        const res = await connectorsApi.startOAuth(id, {
          client_id: client,
          redirect_uri: connectorsApi.oauthCallbackUrl(),
        });
        if (popup) popup.location.href = res.authorize_url;
        else window.open(res.authorize_url, '_blank', 'noopener');
        setStep(4);
      } catch (err) {
        popup?.close();
        setError(err instanceof Error ? err : new Error(String(err)));
      } finally {
        setBusy(false);
      }
    })();
  }

  // Best-effort kick of an immediate first sync. The scheduler will
  // also sync on its own cadence once OAuth completes, so a failure
  // here (e.g. auth not finished yet) is surfaced but non-fatal.
  function runFirstSync() {
    if (!instanceId || busy) return;
    setBusy(true);
    setError(undefined);
    void connectorsApi
      .syncConnector(instanceId)
      .catch((err: unknown) =>
        setError(err instanceof Error ? err : new Error(String(err))),
      )
      .finally(() => setBusy(false));
  }

  return (
    <div>
      <PageHeader
        title="Welcome to Knowledge"
        description="Let's connect your first source and start building memory."
        actions={
          step !== 4 ? (
            <button className="btn-sm" onClick={skip}>
              Skip for now
            </button>
          ) : undefined
        }
      />

      <WizardSteps current={step} />
      <ErrorBanner error={error} />

      {step === 1 && (
        <Card title="1 · What happens next">
          <p className="muted" style={{ marginTop: 0 }}>
            Knowledge ingests content from the tools you already use,
            extracts entities and facts, and synthesizes decaying memory
            you can search. To get value you need at least one connector.
          </p>
          <p className="muted">
            This wizard registers a connector, walks you through its
            OAuth sign-in, and schedules the first sync. It takes about a
            minute.
          </p>
          <button className="btn-primary" onClick={() => setStep(2)}>
            Get started
          </button>
        </Card>
      )}

      {step === 2 && (
        <Card title="2 · Choose a source">
          <p className="muted" style={{ marginTop: 0 }}>
            Pick the system to connect first. You can add more later from
            the Connectors page.
          </p>
          <div className="kind-grid">
            {WIZARD_CONNECTOR_KINDS.map((k) => (
              <button
                key={k}
                className={
                  kind === k ? 'kind-tile kind-tile-active' : 'kind-tile'
                }
                onClick={() => pickKind(k)}
              >
                {connectorLabel(k)}
              </button>
            ))}
          </div>
          <div className="wizard-actions">
            <button onClick={() => setStep(1)}>Back</button>
          </div>
        </Card>
      )}

      {step === 3 && kind && (
        <Card title={`3 · Connect ${connectorLabel(kind)}`}>
          <form onSubmit={connect}>
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
              <label>OAuth client ID</label>
              <input
                value={clientId}
                onChange={(e) => setClientId(e.target.value)}
                placeholder={`client_id for your ${connectorLabel(kind)} app`}
                required
              />
            </div>
            <p className="muted">
              We'll register the connector and open{' '}
              {connectorLabel(kind)}'s sign-in page in a new tab to
              authorize access.
            </p>
            <div className="wizard-actions">
              <button type="button" onClick={() => setStep(2)} disabled={busy}>
                Back
              </button>
              <button className="btn-primary" type="submit" disabled={busy}>
                {busy ? 'Connecting…' : 'Connect & authorize'}
              </button>
            </div>
          </form>
        </Card>
      )}

      {step === 4 && (
        <Card title="4 · You're set">
          <p className="muted" style={{ marginTop: 0 }}>
            {kind ? connectorLabel(kind) : 'Your connector'} is registered
            and its first sync is scheduled. Once you finish sign-in in
            the other tab, content will start flowing and synthesis will
            run automatically.
          </p>
          <div className="wizard-actions">
            <button onClick={runFirstSync} disabled={busy || !instanceId}>
              {busy ? 'Starting sync…' : 'Run first sync now'}
            </button>
            <button className="btn-primary" onClick={finish}>
              Go to Dashboard
            </button>
          </div>
        </Card>
      )}
    </div>
  );
}

const STEP_LABELS = ['Welcome', 'Source', 'Connect', 'Done'];

function WizardSteps({ current }: { current: Step }) {
  return (
    <div className="wizard-steps">
      {STEP_LABELS.map((label, i) => {
        const n = i + 1;
        const cls =
          n === current
            ? 'wizard-step wizard-step-active'
            : n < current
              ? 'wizard-step wizard-step-done'
              : 'wizard-step';
        return (
          <div key={label} className={cls}>
            <span className="wizard-step-num">{n}</span>
            <span>{label}</span>
          </div>
        );
      })}
    </div>
  );
}

/**
 * A random scope UUID for the suggested default. Uses the platform
 * `crypto.randomUUID` where available (all evergreen browsers), with a
 * small RFC-4122-shaped fallback for older/embedded webviews.
 */
function freshUuid(): string {
  const c: Crypto | undefined = globalThis.crypto;
  if (c && typeof c.randomUUID === 'function') return c.randomUUID();
  return 'xxxxxxxx-xxxx-4xxx-yxxx-xxxxxxxxxxxx'.replace(/[xy]/g, (ch) => {
    const r = (Math.random() * 16) | 0;
    const v = ch === 'x' ? r : (r & 0x3) | 0x8;
    return v.toString(16);
  });
}
