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
 * The ten popular, named source systems offered in the first-run
 * wizard. Excludes `generic_webhook` — a catch-all transport rather
 * than a recognisable SaaS an SME would pick first — leaving exactly
 * the "top 10" the wizard advertises.
 */
export const POPULAR_CONNECTOR_KINDS: readonly ConnectorKind[] =
  CONNECTOR_KINDS.filter((k) => k !== 'generic_webhook');

/** Display label for a kind, falling back to the raw tag if unknown. */
export function connectorLabel(kind: string): string {
  return (CONNECTOR_LABELS as Record<string, string>)[kind] ?? kind;
}
