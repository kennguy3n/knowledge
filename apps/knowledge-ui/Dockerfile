# syntax=docker/dockerfile:1

# ── Stage 1: build the static export ────────────────────────────────
FROM node:22-alpine AS build
WORKDIR /app

# Install dependencies against the lockfile first for better layer
# caching. `npm ci` requires package-lock.json; fall back to install
# when the lockfile is absent.
COPY package.json package-lock.json* ./
RUN if [ -f package-lock.json ]; then npm ci; else npm install; fi

COPY . .
# NEXT_PUBLIC_GATEWAY_BASE_URL is baked into the bundle at build time.
# Leave empty (default) so the UI calls the gateway same-origin via the
# nginx reverse proxy below.
ARG NEXT_PUBLIC_GATEWAY_BASE_URL=""
ENV NEXT_PUBLIC_GATEWAY_BASE_URL=${NEXT_PUBLIC_GATEWAY_BASE_URL}
RUN npm run build

# ── Stage 2: serve with nginx ───────────────────────────────────────
FROM nginx:1.27-alpine AS serve

# nginx.conf is an envsubst template: the official entrypoint renders
# /etc/nginx/templates/*.template → /etc/nginx/conf.d/ on start. Only
# GATEWAY_*-prefixed vars are substituted (NGINX_ENVSUBST_FILTER) so
# nginx's own $host/$uri runtime vars survive. Override GATEWAY_UPSTREAM
# to point the reverse proxy at a different gateway address (e.g. the
# Service DNS name under Helm).
COPY nginx.conf /etc/nginx/templates/default.conf.template
COPY --from=build /app/out /usr/share/nginx/html

ENV GATEWAY_UPSTREAM=knowledge-gateway:8080 \
    GATEWAY_RESOLVER=127.0.0.11 \
    NGINX_ENVSUBST_FILTER=GATEWAY_

# The container listens on 3002 to match the docker-compose port
# mapping and the dev server port.
EXPOSE 3002
HEALTHCHECK --interval=10s --timeout=3s --start-period=5s --retries=5 \
  CMD wget -qO- http://localhost:3002/ >/dev/null 2>&1 || exit 1

CMD ["nginx", "-g", "daemon off;"]
