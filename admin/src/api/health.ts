import { request } from './http';
import type { GatewayHealth } from './types';

/** `GET /health` — gateway aggregate health (unauthenticated). */
export function getHealth(signal?: AbortSignal): Promise<GatewayHealth> {
  return request<GatewayHealth>('/health', { signal });
}

/**
 * `GET /metrics/knowledge` — Prometheus text exposition of the
 * `knowledge_*` counters/gauges. Returned as raw text; the Dashboard
 * parses a handful of well-known series for headline tiles.
 */
export async function getKnowledgeMetricsText(
  signal?: AbortSignal,
): Promise<string> {
  const res = await fetch('/metrics/knowledge', {
    headers: { Accept: 'text/plain' },
    signal,
  });
  if (!res.ok) throw new Error(`metrics: ${res.status} ${res.statusText}`);
  return res.text();
}

/** A single parsed Prometheus sample (labels collapsed to a string). */
export interface MetricSample {
  name: string;
  labels: string;
  value: number;
}

/**
 * Parse Prometheus text exposition into flat samples. This is a
 * deliberately small parser: it ignores `# HELP` / `# TYPE` lines and
 * does not attempt to model histograms beyond their `_bucket`/`_sum`/
 * `_count` series, which is sufficient for the headline tiles.
 */
export function parsePrometheus(text: string): MetricSample[] {
  const out: MetricSample[] = [];
  for (const line of text.split('\n')) {
    const trimmed = line.trim();
    if (!trimmed || trimmed.startsWith('#')) continue;
    const braceIdx = trimmed.indexOf('{');
    let name: string;
    let labels = '';
    let rest: string;
    if (braceIdx !== -1) {
      name = trimmed.slice(0, braceIdx);
      const closeIdx = trimmed.indexOf('}', braceIdx);
      if (closeIdx === -1) continue;
      labels = trimmed.slice(braceIdx + 1, closeIdx);
      rest = trimmed.slice(closeIdx + 1).trim();
    } else {
      const sp = trimmed.indexOf(' ');
      if (sp === -1) continue;
      name = trimmed.slice(0, sp);
      rest = trimmed.slice(sp + 1).trim();
    }
    const value = Number(rest.split(/\s+/)[0]);
    if (Number.isNaN(value)) continue;
    out.push({ name, labels, value });
  }
  return out;
}
