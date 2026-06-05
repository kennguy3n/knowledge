'use client';

import { useEffect, useRef, useState } from 'react';
import type { SynthesisRecord } from '@/lib/types';
import { streamSynthesisStatus, synthesisStatus, triggerSynthesis } from '@/lib/api';
import { StatusBadge } from './ui';

const TERMINAL = new Set(['succeeded', 'success', 'failed', 'error', 'completed', 'done']);

function isTerminal(status: string | undefined): boolean {
  return status ? TERMINAL.has(status.toLowerCase()) : false;
}

function progressOf(record: SynthesisRecord | undefined): number | undefined {
  if (!record) return undefined;
  const p = record.progress;
  if (typeof p !== 'number' || Number.isNaN(p)) return undefined;
  // Accept either a 0..1 fraction or a 0..100 percentage.
  return p > 1 ? Math.min(100, p) / 100 : Math.max(0, Math.min(1, p));
}

/**
 * Trigger a synthesis run for a scope and stream its progress over SSE
 * (`GET /api/v1/synthesis/{id}/status?stream=true`). Falls back to a
 * one-shot snapshot if the stream ends without a terminal status.
 */
export function SynthesisStatus({
  scopeId,
  onComplete,
}: {
  scopeId: string;
  onComplete?: () => void;
}) {
  const [record, setRecord] = useState<SynthesisRecord | undefined>();
  const [running, setRunning] = useState(false);
  const [error, setError] = useState<string | undefined>();
  const closeRef = useRef<(() => void) | null>(null);

  // Tear down any open stream on unmount.
  useEffect(() => () => closeRef.current?.(), []);

  async function run() {
    setError(undefined);
    setRunning(true);
    setRecord(undefined);
    // The gateway sends a terminal `status` frame immediately followed by
    // `done`, so both onStatus and onDone observe completion. Guard so
    // onComplete (which refreshes the parent's memory list) fires once
    // per run rather than once per signal.
    let completed = false;
    const complete = () => {
      if (completed) return;
      completed = true;
      onComplete?.();
    };
    try {
      const started = await triggerSynthesis({
        scope_id: scopeId,
        trigger: 'ManualUserAction',
      });
      setRecord(started);
      const id = started.id;
      if (!id) {
        // Substrate did not return an id to follow; surface what we got.
        setRunning(false);
        return;
      }
      closeRef.current = streamSynthesisStatus(id, {
        onStatus: (r) => {
          setRecord(r);
          if (isTerminal(r.status)) {
            setRunning(false);
            complete();
          }
        },
        onDone: async () => {
          closeRef.current = null;
          // Reconcile with a final snapshot in case the terminal status
          // arrived in the same frame as `done`.
          try {
            const snap = await synthesisStatus(id);
            setRecord(snap);
          } catch {
            // ignore — keep the last streamed record
          }
          setRunning(false);
          complete();
        },
        onError: (e) => {
          setError(e.message);
          setRunning(false);
        },
      });
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      setRunning(false);
    }
  }

  const progress = progressOf(record);

  return (
    <div className="synthesis-status">
      <div className="synthesis-status-head">
        <button className="btn btn-primary" onClick={run} disabled={running}>
          {running ? 'Synthesizing…' : 'Synthesize now'}
        </button>
        {record?.status && <StatusBadge status={record.status} />}
      </div>

      {progress !== undefined && (
        <div className="meter" aria-hidden="true">
          <div className="meter-fill" style={{ width: `${progress * 100}%` }} />
        </div>
      )}

      {record?.detail && <p className="muted small">{record.detail}</p>}
      {error && <div className="banner banner-error">{error}</div>}
    </div>
  );
}
