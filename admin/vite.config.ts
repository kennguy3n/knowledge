import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';

// During local development the admin SPA talks to the gateway. By
// default it proxies same-origin `/api`, `/health`, and `/metrics`
// requests to the gateway at http://localhost:8080 so `npm run dev`
// works against a locally running stack without CORS configuration.
// Override the target with KNOWLEDGE_GATEWAY_URL when the gateway
// listens elsewhere.
const gateway = process.env.KNOWLEDGE_GATEWAY_URL ?? 'http://localhost:8080';

export default defineConfig({
  plugins: [react()],
  server: {
    port: 3001,
    proxy: {
      '/api': { target: gateway, changeOrigin: true },
      '/health': { target: gateway, changeOrigin: true },
      '/metrics': { target: gateway, changeOrigin: true },
    },
  },
});
