'use client';

import { usePathname } from 'next/navigation';

/**
 * Resolve the chat scope id from the current `/chat/<scopeId>` URL.
 *
 * The route param is read from `usePathname()` rather than `useParams()`
 * because, under static export, every `/chat/<id>` deep link is served
 * by the same placeholder template; the real id only exists in the
 * browser URL. Reading the pathname makes both in-app navigation and
 * hard refreshes resolve the correct scope.
 */
export function useScopeId(): string {
  const pathname = usePathname() ?? '';
  const segments = pathname.split('/').filter(Boolean);
  // ['chat', '<scopeId>']
  if (segments[0] !== 'chat' || segments.length < 2) return '';
  try {
    return decodeURIComponent(segments[1]);
  } catch {
    return segments[1];
  }
}
