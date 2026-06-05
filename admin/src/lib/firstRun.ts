/**
 * First-run wizard dismissal state.
 *
 * The wizard auto-opens once when the deployment has no connectors yet
 * (see `App`). After the operator finishes or skips it we persist a
 * flag so it does not reappear on every visit to an empty deployment —
 * they can always reopen it from the Dashboard's "Getting started"
 * card. `localStorage` is per-browser, which is the right scope: this
 * is a UI onboarding hint, not server state.
 */
const FIRST_RUN_DISMISSED_KEY = 'knowledge.admin.firstRunDismissed';

/** Whether the operator has finished or skipped the first-run wizard. */
export function isFirstRunDismissed(): boolean {
  try {
    return localStorage.getItem(FIRST_RUN_DISMISSED_KEY) === '1';
  } catch {
    // Private-mode / disabled storage: treat as not dismissed but never
    // throw — the wizard simply may reappear, which is harmless.
    return false;
  }
}

/** Record that the wizard has been finished or skipped. */
export function markFirstRunDismissed(): void {
  try {
    localStorage.setItem(FIRST_RUN_DISMISSED_KEY, '1');
  } catch {
    // Ignore storage failures (see `isFirstRunDismissed`).
  }
}
