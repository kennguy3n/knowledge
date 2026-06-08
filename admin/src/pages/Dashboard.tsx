import { useMemo } from 'react';
import { Link } from 'react-router-dom';
import { connectorsApi, healthApi } from '../api';
import type {
  AdapterReport,
  GatewayHealth,
  SubstrateHealth,
  SubsystemHealth,
} from '../api';
import { useAsync } from '../hooks/useAsync';
import {
  Card,
  ErrorBanner,
  JsonBlock,
  Notice,
  PageHeader,
  Spinner,
  StatusBadge,
  Tile,
} from '../components/ui';

function isSubstrateHealth(v: unknown): v is SubstrateHealth {
  return typeof v === 'object' && v !== null && 'subsystems' in v;
}

/** Flatten the gateway health envelope into a list of {name, status}. */
function flattenSubsystems(
  health: GatewayHealth | undefined,
): { name: string; status: string; detail?: string }[] {
  if (!health) return [];
  const rows: { name: string; status: string; detail?: string }[] = [];
  for (const [name, value] of Object.entries(health.subsystems)) {
    if (name === 'substrate_detail') continue;
    if (typeof value === 'string') {
      rows.push({ name, status: value });
    }
  }
  const detail = health.subsystems['substrate_detail'];
  if (isSubstrateHealth(detail)) {
    for (const sub of detail.subsystems) {
      rows.push({
        name: `substrate / ${sub.name}`,
        status: sub.status,
        detail: sub.detail ?? undefined,
      });
    }
  }
  return rows;
}

function substrateDetail(
  health: GatewayHealth | undefined,
): SubstrateHealth | undefined {
  const d = health?.subsystems['substrate_detail'];
  return isSubstrateHealth(d) ? d : undefined;
}

export default function Dashboard() {
  const health = useAsync((signal) => healthApi.getHealth(signal), []);
  const metrics = useAsync(
    (signal) => healthApi.getKnowledgeMetricsText(signal),
    [],
  );
  const connectors = useAsync(
    (signal) => connectorsApi.listConnectors(signal),
    [],
  );

  // Nudge operators who haven't finished onboarding. Only shown once
  // we positively know the count (a failed list leaves data undefined
  // and hides the card rather than misreporting "0 connectors").
  const connectorCount = connectors.data?.length;
  const showGettingStarted =
    connectorCount !== undefined && connectorCount < 3;

  const subsystems = useMemo(
    () => flattenSubsystems(health.data),
    [health.data],
  );
  const detail = useMemo(() => substrateDetail(health.data), [health.data]);

  const headline = useMemo(
    () => (metrics.data ? headlineMetrics(metrics.data) : []),
    [metrics.data],
  );

  const overall = health.data?.status ?? (health.error ? 'down' : 'unknown');

  return (
    <div>
      <PageHeader
        title="Dashboard"
        description="Aggregate health of the gateway, substrate, and downstream subsystems."
        actions={
          <button className="btn-sm" onClick={() => { health.reload(); metrics.reload(); }}>
            Refresh
          </button>
        }
      />

      <ErrorBanner error={health.error} />
      {metrics.error && (
        <Notice>
          Headline metrics unavailable: {metrics.error.message}. Health and
          subsystem status below are unaffected.
        </Notice>
      )}

      {showGettingStarted && (
        <Card title="Getting started">
          <p className="muted" style={{ marginTop: 0 }}>
            {connectorCount === 0
              ? 'No connectors are configured yet — Knowledge has nothing to ingest.'
              : `Only ${connectorCount} connector${connectorCount === 1 ? '' : 's'} configured. Connect a few sources to get the most out of synthesis.`}{' '}
            The setup wizard walks you through connecting a source in about
            a minute.
          </p>
          <Link className="btn btn-primary" to="/welcome">
            Open setup wizard
          </Link>
        </Card>
      )}

      <div className="tiles">
        <Tile
          label="Overall status"
          value={<StatusBadge status={overall} />}
          tone={overall === 'ok' ? 'ok' : overall === 'degraded' ? 'warn' : 'bad'}
        />
        {detail && (
          <>
            <Tile label="Core version" value={detail.core_version} />
            <Tile
              label="Uptime"
              value={formatUptime(detail.uptime_secs)}
            />
            <Tile
              label="Tracing"
              value={detail.tracing_initialized ? 'on' : 'off'}
              tone={detail.tracing_initialized ? 'ok' : 'warn'}
            />
          </>
        )}
        {headline.map((m) => (
          <Tile key={m.label} label={m.label} value={m.value} />
        ))}
      </div>

      <Card title="Subsystems">
        {health.loading && <Spinner />}
        {!health.loading && subsystems.length === 0 && (
          <p className="muted">No subsystem data reported.</p>
        )}
        {subsystems.length > 0 && (
          <table>
            <thead>
              <tr>
                <th>Subsystem</th>
                <th>Status</th>
                <th>Detail</th>
              </tr>
            </thead>
            <tbody>
              {subsystems.map((s) => (
                <tr key={s.name}>
                  <td className="mono">{s.name}</td>
                  <td>
                    <StatusBadge status={s.status} />
                  </td>
                  <td className="muted">{s.detail ?? '—'}</td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </Card>

      <Card title="Inference adapters">
        <AdapterTable subsystems={detail?.subsystems} />
      </Card>

      <Card title="Raw health document">
        {health.data ? (
          <JsonBlock value={health.data} />
        ) : health.loading ? (
          <Spinner />
        ) : (
          <p className="muted">No health document — the request failed.</p>
        )}
      </Card>
    </div>
  );
}

function adapterStatus(a: AdapterReport): string {
  if (!a.available) return 'unavailable';
  return a.loaded ? 'ok' : 'degraded';
}

function AdapterTable({ subsystems }: { subsystems?: SubsystemHealth[] }) {
  const router = subsystems?.find((s) => s.name === 'inference_router');
  const adapters = router?.adapters;
  if (!adapters || adapters.length === 0) {
    return <p className="muted">No per-adapter report available.</p>;
  }
  return (
    <table>
      <thead>
        <tr>
          <th>Adapter</th>
          <th>Status</th>
          <th>Loaded</th>
          <th>Supports</th>
        </tr>
      </thead>
      <tbody>
        {adapters.map((a) => (
          <tr key={a.kind}>
            <td className="mono">{a.kind}</td>
            <td>
              <StatusBadge status={adapterStatus(a)} />
            </td>
            <td>{a.loaded ? 'yes' : 'no'}</td>
            <td className="muted">{a.supports.join(', ') || '—'}</td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}

/** Pull a few well-known counters out of the Prometheus text. */
function headlineMetrics(text: string): { label: string; value: string }[] {
  const samples = healthApi.parsePrometheus(text);
  const sum = (prefix: string) =>
    samples
      .filter((s) => s.name === prefix)
      .reduce((acc, s) => acc + s.value, 0);
  const pairs: { label: string; metric: string }[] = [
    { label: 'Ingested', metric: 'knowledge_ingest_total' },
    { label: 'Queries', metric: 'knowledge_query_total' },
    { label: 'Synthesis triggers', metric: 'knowledge_synthesis_trigger_total' },
  ];
  const out: { label: string; value: string }[] = [];
  for (const p of pairs) {
    const v = sum(p.metric);
    if (samples.some((s) => s.name === p.metric)) {
      out.push({ label: p.label, value: String(v) });
    }
  }
  return out;
}

function formatUptime(secs: number): string {
  if (secs < 60) return `${secs}s`;
  if (secs < 3600) return `${Math.floor(secs / 60)}m`;
  if (secs < 86400) return `${Math.floor(secs / 3600)}h`;
  return `${Math.floor(secs / 86400)}d`;
}
