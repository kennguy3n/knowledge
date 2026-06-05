import type { ReactNode } from 'react';

/** Page header with a title and optional action buttons on the right. */
export function PageHeader({
  title,
  description,
  actions,
}: {
  title: string;
  description?: string;
  actions?: ReactNode;
}) {
  return (
    <div className="page-header">
      <div>
        <h1>{title}</h1>
        {description && <p className="muted">{description}</p>}
      </div>
      {actions && <div className="page-header-actions">{actions}</div>}
    </div>
  );
}

/** Red banner for an error, with the parsed message. */
export function ErrorBanner({ error }: { error: Error | undefined }) {
  if (!error) return null;
  return (
    <div className="banner banner-error" role="alert">
      <strong>Error:</strong> {error.message}
    </div>
  );
}

/** Neutral banner for empty / placeholder states. */
export function Notice({ children }: { children: ReactNode }) {
  return <div className="banner banner-notice">{children}</div>;
}

export function Spinner({ label = 'Loading…' }: { label?: string }) {
  return <div className="spinner" aria-busy="true">{label}</div>;
}

/** Coloured pill for a status string. */
export function StatusBadge({ status }: { status: string }) {
  const tone = statusTone(status);
  return <span className={`badge badge-${tone}`}>{status}</span>;
}

function statusTone(status: string): 'ok' | 'warn' | 'bad' | 'neutral' {
  const s = status.toLowerCase();
  if (['ok', 'active', 'succeeded', 'reinforced'].includes(s)) return 'ok';
  if (['degraded', 'in_progress', 'invited', 'candidate', 'full'].includes(s))
    return 'warn';
  if (['down', 'unavailable', 'failed', 'suspended'].includes(s)) return 'bad';
  return 'neutral';
}

/** Card container. */
export function Card({
  title,
  children,
}: {
  title?: string;
  children: ReactNode;
}) {
  return (
    <section className="card">
      {title && <h2 className="card-title">{title}</h2>}
      {children}
    </section>
  );
}

/** Headline metric tile. */
export function Tile({
  label,
  value,
  tone = 'neutral',
}: {
  label: string;
  value: ReactNode;
  tone?: 'ok' | 'warn' | 'bad' | 'neutral';
}) {
  return (
    <div className={`tile tile-${tone}`}>
      <div className="tile-value">{value}</div>
      <div className="tile-label">{label}</div>
    </div>
  );
}

/** Pretty-printed JSON block. */
export function JsonBlock({ value }: { value: unknown }) {
  return <pre className="json-block">{JSON.stringify(value, null, 2)}</pre>;
}
