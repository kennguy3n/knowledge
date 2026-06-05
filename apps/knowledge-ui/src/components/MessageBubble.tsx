import { formatTimestamp } from '@/lib/format';

export interface ChatMessage {
  id: string;
  /** "user" messages are typed locally; "system" are status notes. */
  role: 'user' | 'system';
  body: string;
  /** Unix epoch milliseconds. */
  at: number;
  /** Evidence id returned by the gateway once ingested (user messages). */
  evidenceId?: string;
  pending?: boolean;
  error?: boolean;
}

/** A single chat message rendered as a bubble. */
export function MessageBubble({ message }: { message: ChatMessage }) {
  const cls = [
    'bubble',
    `bubble-${message.role}`,
    message.pending ? 'bubble-pending' : '',
    message.error ? 'bubble-error' : '',
  ]
    .filter(Boolean)
    .join(' ');

  return (
    <div className={cls}>
      <div className="bubble-body">{message.body}</div>
      <div className="bubble-meta">
        <span>{formatTimestamp(Math.round(message.at / 1000))}</span>
        {message.pending && <span> · sending…</span>}
        {message.error && <span> · failed to ingest</span>}
        {message.evidenceId && (
          <span title={message.evidenceId}> · ingested</span>
        )}
      </div>
    </div>
  );
}
