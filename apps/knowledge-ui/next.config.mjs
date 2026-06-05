/** @type {import('next').NextConfig} */
//
// The UI is shipped as a fully static export (`output: 'export'`) so it
// can be served by nginx exactly like the `admin/` SPA — no Node runtime
// in the production image. All gateway calls happen client-side against
// the same origin (nginx reverse-proxies `/api` and `/health` to the
// gateway), so there is no server-rendering dependency on the API.
//
// `trailingSlash` makes every route emit a directory `index.html`, which
// keeps nginx's `try_files $uri $uri/ …` happy. `images.unoptimized` is
// required because the static export has no image-optimization server.
//
// `skipTrailingSlashRedirect` stops `next dev`/`next start` from issuing a
// 308 Permanent Redirect that rewrites a no-slash path to its trailing-slash
// form. Without it, a same-origin gateway call like `/api/v1/memories` is
// 308'd to `/api/v1/memories/` during local dev — and because browsers cache
// 308s *permanently*, that stale redirect then replays against the production
// nginx build on the same origin, where the gateway has no trailing-slash
// route and returns 404. The static export still emits directory-style
// `index.html` pages from `trailingSlash`; this only suppresses the runtime
// redirect, which the gateway proxy must never receive.
//
// Export mode is gated to production builds only. The chat route
// (`/chat/[scopeId]`) takes a runtime-only UUID; under `output: 'export'`
// the dev server tries to statically generate whatever id is visited and
// errors. Disabling export in `next dev` (NODE_ENV !== 'production') lets
// the dynamic route render normally for any scope while `next build`
// still emits the static export served by nginx in production.
const nextConfig = {
  output: process.env.NODE_ENV === 'production' ? 'export' : undefined,
  trailingSlash: true,
  skipTrailingSlashRedirect: true,
  images: { unoptimized: true },
  reactStrictMode: true,
};

export default nextConfig;
