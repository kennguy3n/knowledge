// Flat ESLint config (ESLint 9) for the Next.js end-user UI.
//
// This replaces the legacy `.eslintrc.json` (`extends: ["next/core-web-vitals"]`).
// ESLint 9 defaults to flat config and no longer reads `.eslintrc.*`.
// eslint-config-next 15 is still an eslintrc-style shareable config, so it is
// loaded through `FlatCompat` — the approach `create-next-app` generates for
// Next 15. Linting runs via the ESLint CLI (`eslint .`) instead of the
// deprecated `next lint`.
import { dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { FlatCompat } from '@eslint/eslintrc';

const __filename = fileURLToPath(import.meta.url);
const __dirname = dirname(__filename);

const compat = new FlatCompat({ baseDirectory: __dirname });

const eslintConfig = [
  {
    ignores: ['.next/**', 'out/**', 'next-env.d.ts'],
  },
  ...compat.extends('next/core-web-vitals'),
];

export default eslintConfig;
