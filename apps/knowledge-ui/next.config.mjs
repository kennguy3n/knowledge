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
// Export mode is gated to production builds only. The chat route
// (`/chat/[scopeId]`) takes a runtime-only UUID; under `output: 'export'`
// the dev server tries to statically generate whatever id is visited and
// errors. Disabling export in `next dev` (NODE_ENV !== 'production') lets
// the dynamic route render normally for any scope while `next build`
// still emits the static export served by nginx in production.
const nextConfig = {
  output: process.env.NODE_ENV === 'production' ? 'export' : undefined,
  trailingSlash: true,
  images: { unoptimized: true },
  reactStrictMode: true,
};

export default nextConfig;
