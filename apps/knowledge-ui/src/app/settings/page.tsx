'use client';

import { useEffect, useState } from 'react';
import { gatewayBaseUrl, getToken, health, setToken } from '@/lib/api';
import type { GatewayHealth } from '@/lib/types';
import { Card, ErrorBanner, PageHeader, StatusBadge } from '@/components/ui';

type Theme = 'light' | 'dark';
const THEME_KEY = 'knowledge.ui.theme';

function applyTheme(theme: Theme) {
  document.documentElement.setAttribute('data-theme', theme);
  try {
    localStorage.setItem(THEME_KEY, theme);
  } catch {
    // ignore persistence failure
  }
}

export default function SettingsPage() {
  const [token, setTokenValue] = useState('');
  const [saved, setSaved] = useState(false);
  const [theme, setTheme] = useState<Theme>('dark');

  const [healthData, setHealthData] = useState<GatewayHealth | undefined>();
  const [healthError, setHealthError] = useState<Error | undefined>();
  const [checking, setChecking] = useState(false);

  useEffect(() => {
    setTokenValue(getToken());
    const current = document.documentElement.getAttribute('data-theme');
    setTheme(current === 'light' ? 'light' : 'dark');
  }, []);

  function saveToken() {
    setToken(token.trim());
    setSaved(true);
    setTimeout(() => setSaved(false), 1500);
  }

  function changeTheme(next: Theme) {
    setTheme(next);
    applyTheme(next);
  }

  async function checkHealth() {
    setChecking(true);
    setHealthError(undefined);
    setHealthData(undefined);
    try {
      setHealthData(await health());
    } catch (e) {
      setHealthError(e instanceof Error ? e : new Error(String(e)));
    } finally {
      setChecking(false);
    }
  }

  const base = gatewayBaseUrl();

  return (
    <div className="page">
      <PageHeader
        title="Settings"
        description="Configure your access token and appearance."
      />

      <Card title="Access token">
        <p className="muted small">
          The gateway accepts a static API key or a tenant JWT as a bearer
          token. It is stored only in this browser’s localStorage and sent as{' '}
          <code>Authorization: Bearer &lt;token&gt;</code>.
        </p>
        <div className="inline-form">
          <input
            className="input"
            type="password"
            placeholder="paste API key or JWT (leave empty for unauthenticated dev)"
            value={token}
            onChange={(e) => setTokenValue(e.target.value)}
          />
          <button className="btn btn-primary" onClick={saveToken}>
            {saved ? 'Saved' : 'Save'}
          </button>
        </div>
      </Card>

      <Card title="Appearance">
        <div className="theme-toggle">
          <button
            className={theme === 'dark' ? 'btn btn-primary' : 'btn'}
            onClick={() => changeTheme('dark')}
          >
            Dark
          </button>
          <button
            className={theme === 'light' ? 'btn btn-primary' : 'btn'}
            onClick={() => changeTheme('light')}
          >
            Light
          </button>
        </div>
      </Card>

      <Card title="Gateway">
        <p className="muted small">
          API base URL: <code>{base || '(same origin)'}</code>
        </p>
        <button className="btn" onClick={checkHealth} disabled={checking}>
          {checking ? 'Checking…' : 'Check health'}
        </button>
        <ErrorBanner error={healthError} />
        {healthData && (
          <div className="health-report">
            <div className="health-overall">
              Overall: <StatusBadge status={healthData.status} />
            </div>
            <ul className="health-subsystems">
              {Object.entries(healthData.subsystems ?? {}).map(([name, val]) => (
                <li key={name}>
                  <span className="mono">{name}</span>{' '}
                  <StatusBadge
                    status={typeof val === 'string' ? val : 'detail'}
                  />
                </li>
              ))}
            </ul>
          </div>
        )}
      </Card>
    </div>
  );
}
