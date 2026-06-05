import { ChatView } from './ChatView';

// The scope is a runtime-only UUID, so `output: export` can't enumerate
// every route. generateStaticParams emits a single placeholder page
// (`/chat/scope/`) that nginx serves as the fallback for every
// `/chat/<id>` deep link; the client component reads the *real* scope id
// from the URL at runtime.
export function generateStaticParams() {
  return [{ scopeId: 'scope' }];
}

// dynamicParams MUST stay true (the default): with `false`, Next's router
// treats any param not returned above as a hard 404 — in `next dev` it
// refuses on-demand rendering, and in the static export the client router
// rejects the route instead of falling back to a hard navigation that
// nginx could serve. true lets dev render any UUID and lets the exported
// client fall through to the nginx placeholder for unknown scopes.
export const dynamicParams = true;

export default function ChatPage() {
  return <ChatView />;
}
