// Small presentation helpers shared across pages/components.

/** Format a Unix epoch (seconds) or ISO string as a local datetime. */
export function formatTimestamp(value: string | number | undefined): string {
  if (value === undefined || value === null || value === '') return '—';
  const date =
    typeof value === 'number'
      ? new Date(value * 1000)
      : new Date(/^\d+$/.test(value) ? Number(value) * 1000 : value);
  if (Number.isNaN(date.getTime())) return String(value);
  return date.toLocaleString();
}

/** Format a [0, 1] score as a percentage string. */
export function formatScore(score: number | undefined): string {
  if (typeof score !== 'number' || Number.isNaN(score)) return '—';
  return `${Math.round(score * 100)}%`;
}

/** RFC-4122 v4 UUID check (matches the gateway's scope_id validation). */
const UUID_RE =
  /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

export function isUuid(value: string): boolean {
  return UUID_RE.test(value.trim());
}

/** Best-effort UUID v4 generator (crypto.randomUUID with a fallback). */
export function newUuid(): string {
  if (
    typeof crypto !== 'undefined' &&
    typeof crypto.randomUUID === 'function'
  ) {
    return crypto.randomUUID();
  }
  // Fallback for non-secure contexts: RFC-4122 v4 from Math.random.
  return 'xxxxxxxx-xxxx-4xxx-yxxx-xxxxxxxxxxxx'.replace(/[xy]/g, (c) => {
    const r = (Math.random() * 16) | 0;
    const v = c === 'x' ? r : (r & 0x3) | 0x8;
    return v.toString(16);
  });
}
