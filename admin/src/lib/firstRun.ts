/**
 * First-run wizard dismissal state.
 *
 * The wizard auto-opens once when the deployment has no connectors yet
 * (see `App`). After the operator finishes or skips it we persist a
 * flag so it does not reappear on every visit to an empty deployment —
 * they can always reopen it from the Dashboard's "Getting started"
 * card. `localStorage` is per-browser, which is the right scope: this
 * is a UI onboarding hint, not server state.
 *
 * Persistence is best-effort: when `localStorage` is unavailable
 * (private/incognito modes, restrictive webviews) it can throw or
 * no-op. To keep dismissal meaningful in that case we also hold a
 * session-scoped in-memory flag. Without it, `FirstRunGate`'s
 * empty-deployment redirect would ping-pong `/dashboard` ⇄ `/welcome`
 * after every skip, trapping the operator on a fresh deployment.
 */
const FIRST_RUN_DISMISSED_KEY = 'knowledge.admin.firstRunDismissed';

// Suppresses the redirect for the remainder of the session even when
// persistent storage fails. Resets on a full page reload, at which
// point the wizard may reappear once — but a single skip re-sets this,
// so there is no loop within a session.
let dismissedInMemory = false;

/** Whether the operator has finished or skipped the first-run wizard. */
export function isFirstRunDismissed(): boolean {
  if (dismissedInMemory) {
    return true;
  }
  try {
    return localStorage.getItem(FIRST_RUN_DISMISSED_KEY) === '1';
  } catch {
    // Private-mode / disabled storage: fall back to the in-memory flag
    // above (already false here) and never throw.
    return false;
  }
}

/** Record that the wizard has been finished or skipped. */
export function markFirstRunDismissed(): void {
  // Always record in-memory first so dismissal sticks for this session
  // regardless of whether persistence succeeds.
  dismissedInMemory = true;
  try {
    localStorage.setItem(FIRST_RUN_DISMISSED_KEY, '1');
  } catch {
    // Persistence is best-effort; the in-memory flag is the
    // session-level guarantee (see `isFirstRunDismissed`).
  }
}
