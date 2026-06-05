# syntax=docker/dockerfile:1

# ── Stage 1: build the static bundle ────────────────────────────────
FROM node:22-alpine AS build
WORKDIR /app

# Install dependencies against the lockfile first for better layer
# caching. `npm ci` requires package-lock.json; fall back to install
# when the lockfile is absent.
COPY package.json package-lock.json* ./
RUN if [ -f package-lock.json ]; then npm ci; else npm install; fi

COPY . .
# VITE_GATEWAY_BASE_URL is baked into the bundle at build time. Leave
# empty (default) so the SPA calls the gateway same-origin via the
# nginx reverse proxy below.
ARG VITE_GATEWAY_BASE_URL=""
ENV VITE_GATEWAY_BASE_URL=${VITE_GATEWAY_BASE_URL}
RUN npm run build

# ── Stage 2: serve with nginx ───────────────────────────────────────
FROM nginx:1.27-alpine AS serve

# The container listens on 3001 to match the docker-compose port
# mapping and the dev server port.
COPY nginx.conf /etc/nginx/conf.d/default.conf
COPY --from=build /app/dist /usr/share/nginx/html

EXPOSE 3001
HEALTHCHECK --interval=10s --timeout=3s --start-period=5s --retries=5 \
  CMD wget -qO- http://localhost:3001/ >/dev/null 2>&1 || exit 1

CMD ["nginx", "-g", "daemon off;"]
