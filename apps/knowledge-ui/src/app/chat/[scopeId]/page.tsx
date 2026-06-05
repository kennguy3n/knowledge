import { ChatView } from './ChatView';

// Static export needs at least one param to emit a route template. The
// scope is dynamic (any UUID), so a single placeholder page is emitted
// and nginx falls back to it for every `/chat/<id>` deep link; the
// client component reads the *real* scope id from the URL at runtime.
// In-app navigation (next/link) never hits this fallback — it renders
// the route client-side with the correct param.
export function generateStaticParams() {
  return [{ scopeId: 'scope' }];
}

export const dynamicParams = false;

export default function ChatPage() {
  return <ChatView />;
}
