import type { ConnectorKind } from '../api';

/**
 * Every connector kind the gateway accepts, in display order. Mirrors
 * the `ConnectorKind` union in `api/types.ts` (kept in sync by hand —
 * the exhaustive `CONNECTOR_LABELS` record below fails to compile if a
 * union member is added without a label here).
 */
export const CONNECTOR_KINDS: readonly ConnectorKind[] = [
  'google_drive',
  'one_drive',
  'notion',
  'jira',
  'confluence',
  'git_hub',
  'slack',
  'figma',
  'hub_spot',
  'email',
  'generic_webhook',
];

/** Human-friendly label for a connector kind. */
export const CONNECTOR_LABELS: Record<ConnectorKind, string> = {
  google_drive: 'Google Drive',
  one_drive: 'OneDrive',
  notion: 'Notion',
  jira: 'Jira',
  confluence: 'Confluence',
  git_hub: 'GitHub',
  slack: 'Slack',
  figma: 'Figma',
  hub_spot: 'HubSpot',
  email: 'Email',
  generic_webhook: 'Generic webhook',
};

/**
 * Source systems offered in the first-run wizard. The wizard drives a
 * single hard-coded OAuth2 authorization step, so it may only advertise
 * kinds the gateway can actually mint an authorize URL for. That set is
 * the gateway's OAuth provider registry — `defaultProviders` in
 * `server/internal/connector/oauth.go`; a kind absent there makes
 * `startOAuth` return `400 connector kind has no OAuth2 provider` and
 * dead-ends the SME mid-wizard.
 *
 * This list MUST stay in sync with that registry. `figma`, `hub_spot`,
 * and `email` are deliberately omitted until the gateway wires their
 * providers — `email` additionally needs a Gmail-vs-Microsoft-Graph
 * choice the by-kind authorize map can't express. They remain available
 * from the full Connectors page, which doesn't assume OAuth.
 */
export const WIZARD_CONNECTOR_KINDS: readonly ConnectorKind[] = [
  'google_drive',
  'one_drive',
  'notion',
  'slack',
  'git_hub',
  'jira',
  'confluence',
];

/** Display label for a kind, falling back to the raw tag if unknown. */
export function connectorLabel(kind: string): string {
  return (CONNECTOR_LABELS as Record<string, string>)[kind] ?? kind;
}
