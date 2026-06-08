import { ChatView } from './ChatView';

// The scope is a runtime-only UUID, so `output: export` can't enumerate
// every route. generateStaticParams emits a single placeholder page
// (`/chat/scope/`) that nginx serves as the fallback for every
// `/chat/<id>` deep link; the client component reads the *real* scope id
// from the URL at runtime.
export function generateStaticParams() {
  return [{ scopeId: 'scope' }];
}

// `dynamicParams` is intentionally NOT exported here. It relies on the
// framework default (`true`), which lets `next dev` render any runtime UUID
// on-demand. Do NOT add `export const dynamicParams = true`: Next.js 15 turns
// that into a hard build error under `output: export` (next.config.mjs enables
// export only in production). In the production export, unknown `/chat/<id>`
// deep links fall through to the `/chat/scope/` placeholder via nginx, and the
// client reads the real id from the URL at runtime.

export default function ChatPage() {
  return <ChatView />;
}
