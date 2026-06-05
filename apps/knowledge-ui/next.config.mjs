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
const nextConfig = {
  output: 'export',
  trailingSlash: true,
  images: { unoptimized: true },
  reactStrictMode: true,
};

export default nextConfig;
