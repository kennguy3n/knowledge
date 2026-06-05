#!/usr/bin/env bash
#
# Knowledge — one-command installer for SMEs.
#
# Gets a fresh host from zero to a running stack with sensible defaults:
#   1. checks Docker + the Compose plugin are present,
#   2. generates strong secrets and writes them to `.env` (never
#      clobbering an existing one — rotating the master key would orphan
#      the encrypted store),
#   3. asks whether to enable on-device synthesis (needs 4GB+ RAM),
#   4. pulls the published images and starts the stack,
#   5. waits for the gateway to report healthy,
#   6. prints the URLs to open.
#
# Usage:
#   # from a clone:
#   ./scripts/install.sh
#
#   # or straight from the web (downloads the compose files into ./knowledge):
#   curl -fsSL https://raw.githubusercontent.com/kennguy3n/knowledge/main/scripts/install.sh | bash
#
# Environment overrides (all optional):
#   KNOWLEDGE_SLM_DEVICE_TIER   high|medium|low — skips the synthesis prompt.
#   KNOWLEDGE_HOME              install dir for the curl|bash path (default ./knowledge).
#   KNOWLEDGE_RAW_BASE          raw repo base URL used to fetch compose files.
#   KNOWLEDGE_IMAGE_TAG         published image tag to run (default "latest").
#   KNOWLEDGE_INSTALL_DRY_RUN   1 — do everything except `docker compose up` / health wait.
#   KNOWLEDGE_ASSUME_YES        1 — non-interactive; accept defaults (enables synthesis).
#
set -euo pipefail

# ── Output helpers ───────────────────────────────────────────────────
if [ -t 1 ] && [ -z "${NO_COLOR:-}" ]; then
  C_RESET=$'\033[0m'; C_BOLD=$'\033[1m'; C_BLUE=$'\033[34m'
  C_GREEN=$'\033[32m'; C_YELLOW=$'\033[33m'; C_RED=$'\033[31m'
else
  C_RESET=''; C_BOLD=''; C_BLUE=''; C_GREEN=''; C_YELLOW=''; C_RED=''
fi

log()  { printf '%s\n' "${C_BLUE}==>${C_RESET} ${C_BOLD}$*${C_RESET}"; }
ok()   { printf '%s\n' "${C_GREEN}✓${C_RESET} $*"; }
warn() { printf '%s\n' "${C_YELLOW}!${C_RESET} $*" >&2; }
die()  { printf '%s\n' "${C_RED}✗ $*${C_RESET}" >&2; exit 1; }

have() { command -v "$1" >/dev/null 2>&1; }

# ── Secret generation ────────────────────────────────────────────────
# Emit `n` random bytes as lowercase hex. Hex is intentional: the value
# is embedded unescaped in `.env` and in a Postgres URL, so it must
# contain no characters that need quoting or URL-encoding.
gen_hex() {
  local n="$1"
  if have openssl; then
    openssl rand -hex "$n"
  elif [ -r /dev/urandom ] && have xxd; then
    xxd -l "$n" -p /dev/urandom | tr -d '\n'
  elif [ -r /dev/urandom ] && have od; then
    od -vN "$n" -An -tx1 /dev/urandom | tr -d ' \n'
  else
    die "no random source available (need one of: openssl, xxd, od)"
  fi
}

# ── Locate the Compose files ─────────────────────────────────────────
# Prefer a local clone (the common case). When piped through curl the
# script has no on-disk repo, so fetch just the two compose files into
# KNOWLEDGE_HOME and run the published images from there.
resolve_compose_dir() {
  local source="${BASH_SOURCE[0]:-}"
  if [ -n "$source" ] && [ -f "$source" ]; then
    local script_dir repo_root
    script_dir="$(cd "$(dirname "$source")" && pwd)"
    repo_root="$(cd "$script_dir/.." && pwd)"
    if [ -f "$repo_root/deploy/docker-compose.yml" ]; then
      printf '%s\n' "$repo_root"
      return 0
    fi
  fi

  # Remote path: download the compose files.
  have curl || die "curl is required to download the compose files"
  local home raw
  home="${KNOWLEDGE_HOME:-$PWD/knowledge}"
  raw="${KNOWLEDGE_RAW_BASE:-https://raw.githubusercontent.com/kennguy3n/knowledge/main}"
  mkdir -p "$home/deploy"
  warn "No local checkout found — downloading compose files into $home"
  curl -fsSL "$raw/deploy/docker-compose.yml" -o "$home/deploy/docker-compose.yml" \
    || die "failed to download docker-compose.yml from $raw"
  curl -fsSL "$raw/deploy/docker-compose.images.yml" -o "$home/deploy/docker-compose.images.yml" \
    || die "failed to download docker-compose.images.yml from $raw"
  printf '%s\n' "$home"
}

# ── Prerequisite checks ──────────────────────────────────────────────
check_docker() {
  if ! have docker; then
    die "Docker is not installed. Install it first: https://docs.docker.com/get-docker/"
  fi
  if ! docker compose version >/dev/null 2>&1; then
    die "The Docker Compose plugin is missing. Install Docker Compose v2: https://docs.docker.com/compose/install/"
  fi
  if ! docker info >/dev/null 2>&1; then
    die "Docker is installed but the daemon is not reachable. Start Docker and re-run."
  fi
  ok "Docker $(docker version --format '{{.Server.Version}}' 2>/dev/null || echo 'ok') and Compose plugin detected"
}

# ── .env generation ──────────────────────────────────────────────────
write_env() {
  local env_file="$1" tier="$2"
  if [ -f "$env_file" ]; then
    warn "$env_file already exists — keeping existing secrets (delete it to regenerate)."
    return 0
  fi

  log "Generating secrets and writing $env_file"
  local master pg_pw minio_pw gf_pw
  master="$(gen_hex 32)"   # 64 hex chars — SQLCipher master key.
  pg_pw="$(gen_hex 24)"
  minio_pw="$(gen_hex 24)"
  gf_pw="$(gen_hex 18)"

  # Scope the restrictive umask to a subshell: the 0600 secrets file is
  # created with no world-readable window, and the caller's umask is
  # left untouched once write_env returns.
  (
  umask 077
  cat > "$env_file" <<EOF
# Generated by scripts/install.sh on $(date -u +%Y-%m-%dT%H:%M:%SZ).
# Secrets are unique to this deployment. Keep this file private and back
# up KNOWLEDGE_MASTER_KEY — losing it makes the encrypted store
# unrecoverable.

# ── Substrate ────────────────────────────────────────────────────────
KNOWLEDGE_MASTER_KEY=$master
# Enables on-device SLM synthesis at the "high" tier; "medium"/"low"
# disable it (classification still runs on the free fallback adapter).
KNOWLEDGE_SLM_DEVICE_TIER=$tier

# ── Managed-cloud synthesis (optional) ───────────────────────────────
# Set the URL to route synthesis to an external OpenAI-compatible
# endpoint instead of the local SLM. See docs/operator/configuration.md.
KNOWLEDGE_MANAGED_INFERENCE_URL=
KNOWLEDGE_MANAGED_INFERENCE_KEY=
KNOWLEDGE_MANAGED_INFERENCE_MODEL=gpt-4o-mini

# ── Gateway ──────────────────────────────────────────────────────────
# Left empty for a frictionless localhost start: the admin SPA then
# calls the gateway without a token. Set a value before exposing the
# gateway off-host.
KNOWLEDGE_API_KEY=
KNOWLEDGE_JWT_SECRET=
KNOWLEDGE_PUBLIC_BASE_URL=http://localhost:8080

# ── Postgres ─────────────────────────────────────────────────────────
POSTGRES_USER=knowledge
POSTGRES_PASSWORD=$pg_pw
POSTGRES_DB=knowledge

# ── MinIO ────────────────────────────────────────────────────────────
# Username matches .env.example and the compose default
# (${MINIO_ROOT_USER:-minioadmin}) so an operator who later switches
# between the installer and a hand-edited .env keeps the same root user
# and avoids a credential mismatch against the existing data volume.
MINIO_ROOT_USER=minioadmin
MINIO_ROOT_PASSWORD=$minio_pw
MINIO_BUCKET=knowledge

# ── Grafana ──────────────────────────────────────────────────────────
GF_ADMIN_USER=admin
GF_ADMIN_PASSWORD=$gf_pw

# ── Published image tag ──────────────────────────────────────────────
# The base compose file keys its image refs on KNOWLEDGE_IMAGE_TAG; the
# published-images overlay (deploy/docker-compose.images.yml) keys on
# KNOWLEDGE_VERSION. This installer layers both files, so pin both to the
# same tag to keep the whole stack on one version.
KNOWLEDGE_IMAGE_TAG=${KNOWLEDGE_IMAGE_TAG:-latest}
KNOWLEDGE_VERSION=${KNOWLEDGE_IMAGE_TAG:-latest}
EOF
  )
  ok "Wrote $env_file (mode 600)"
}

# ── Synthesis prompt ─────────────────────────────────────────────────
# Resolve the device tier from (in order): a preset env var, an explicit
# assume-yes, or an interactive prompt read from the controlling
# terminal (so it works even when the script itself arrived on stdin via
# curl|bash). Falls back to enabling synthesis when non-interactive.
resolve_device_tier() {
  if [ -n "${KNOWLEDGE_SLM_DEVICE_TIER:-}" ]; then
    printf '%s\n' "$KNOWLEDGE_SLM_DEVICE_TIER"
    return 0
  fi
  if [ "${KNOWLEDGE_ASSUME_YES:-}" = "1" ]; then
    printf 'high\n'
    return 0
  fi
  if [ -r /dev/tty ]; then
    local reply
    printf '%s' "${C_BOLD}Enable on-device synthesis? It produces summaries/concepts but needs 4GB+ RAM. [Y/n] ${C_RESET}" > /dev/tty
    read -r reply < /dev/tty || reply=""
    case "$reply" in
      [Nn]*) printf 'medium\n' ;;
      *)     printf 'high\n' ;;
    esac
    return 0
  fi
  warn "Non-interactive shell and no KNOWLEDGE_SLM_DEVICE_TIER set — defaulting to synthesis enabled (high)."
  printf 'high\n'
}

# ── Health wait ──────────────────────────────────────────────────────
wait_for_health() {
  local port="$1" tries=60 i=1 url
  url="http://localhost:${port}/health"
  log "Waiting for the gateway to become healthy at $url"
  while [ "$i" -le "$tries" ]; do
    if have curl && curl -fsS "$url" >/dev/null 2>&1; then
      ok "Gateway is healthy"
      return 0
    fi
    if have wget && wget -qO- "$url" >/dev/null 2>&1; then
      ok "Gateway is healthy"
      return 0
    fi
    sleep 2
    i=$((i + 1))
  done
  warn "Gateway did not report healthy within $((tries * 2))s. Check 'docker compose logs -f'."
  return 1
}

# ── Read a var back out of the generated .env ────────────────────────
env_value() {
  local env_file="$1" key="$2" default="$3" line
  line="$(grep -E "^${key}=" "$env_file" 2>/dev/null | tail -n1 || true)"
  if [ -n "$line" ]; then
    printf '%s\n' "${line#*=}"
  else
    printf '%s\n' "$default"
  fi
}

main() {
  printf '%s\n\n' "${C_BOLD}Knowledge installer${C_RESET}"
  check_docker

  local compose_dir env_file tier
  compose_dir="$(resolve_compose_dir)"
  env_file="$compose_dir/.env"
  log "Using compose files in $compose_dir/deploy"

  tier="$(resolve_device_tier)"
  case "$tier" in
    high)         ok "Synthesis enabled (device tier: high)" ;;
    medium|low)   ok "Synthesis disabled (device tier: $tier)" ;;
    *)            die "invalid KNOWLEDGE_SLM_DEVICE_TIER '$tier' (expected high|medium|low)" ;;
  esac

  write_env "$env_file" "$tier"

  local gateway_port admin_port grafana_port
  gateway_port="$(env_value "$env_file" GATEWAY_PORT 8080)"
  admin_port="$(env_value "$env_file" ADMIN_PORT 3001)"
  grafana_port="$(env_value "$env_file" GRAFANA_PORT 3000)"

  # Pass --env-file explicitly: Compose resolves a bare `.env` relative
  # to the compose file's directory (deploy/), not the repo root where
  # we write it, so without this the generated secrets are silently
  # ignored and KNOWLEDGE_MASTER_KEY comes through blank.
  if [ "${KNOWLEDGE_INSTALL_DRY_RUN:-}" = "1" ]; then
    warn "KNOWLEDGE_INSTALL_DRY_RUN=1 — validating compose config, skipping 'up'."
    ( cd "$compose_dir" && docker compose \
        --env-file "$env_file" \
        -f deploy/docker-compose.yml \
        -f deploy/docker-compose.images.yml \
        config >/dev/null ) || die "docker compose config failed"
    ok "Compose configuration is valid"
  else
    log "Pulling images and starting the stack (this can take a few minutes)…"
    ( cd "$compose_dir" && docker compose \
        --env-file "$env_file" \
        -f deploy/docker-compose.yml \
        -f deploy/docker-compose.images.yml \
        up -d ) || die "docker compose up failed"
    wait_for_health "$gateway_port" || true
  fi

  printf '\n%s\n' "${C_GREEN}${C_BOLD}Knowledge is running.${C_RESET}"
  printf '  %s  %s\n' "Admin:" "http://localhost:${admin_port}"
  printf '  %s  %s\n' "API:  " "http://localhost:${gateway_port}"
  printf '  %s  %s\n' "Grafana:" "http://localhost:${grafana_port} (user: admin)"
  printf '\nOpen the Admin URL to finish setup with the first-run wizard.\n'
}

main "$@"
