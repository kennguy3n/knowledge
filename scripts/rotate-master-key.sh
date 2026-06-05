#!/usr/bin/env bash
#
# rotate-master-key.sh — offline master-key rotation for a Docker
# Compose deployment of the Knowledge substrate.
#
# This wraps the `knowledge-rotate-key` binary baked into the substrate
# image (see deploy/Dockerfile.substrate). It:
#
#   1. stops the substrate (and the gateway that writes through it) so
#      the SQLCipher stores are quiescent,
#   2. runs the rotation tool in a one-off container against the SAME
#      `substrate-data` volume (re-keying substrate.db + permissions.db
#      to the new key, keeping `.bak.<unix>` copies under the old key),
#   3. optionally rewrites KNOWLEDGE_MASTER_KEY in your env file and
#      brings the stack back up under the new key.
#
# The rotation is verified (row counts + byte-identical body decrypts)
# before the live files are swapped; on any failure the originals are
# left untouched under the old key. See docs/security/key-rotation.md.
#
# Usage:
#   KNOWLEDGE_MASTER_KEY=<old 64-hex> \
#   KNOWLEDGE_NEW_MASTER_KEY=<new 64-hex> \
#   scripts/rotate-master-key.sh [--env-file PATH] [--yes]
#
# Options:
#   --env-file PATH  Compose env file to read existing vars from and, on
#                    success, rewrite KNOWLEDGE_MASTER_KEY in. When set,
#                    the script also restarts the stack. Defaults to
#                    the repo-root .env if present (the file the
#                    Makefile/compose actually read).
#   --yes            Skip the interactive confirmation prompt.
#   -h, --help       Show this help.
#
# Environment:
#   KNOWLEDGE_MASTER_KEY      Current (old) master key, 64 hex chars.
#   KNOWLEDGE_NEW_MASTER_KEY  New master key, 64 hex chars.
#   COMPOSE_FILE              Compose file (default: deploy/docker-compose.yml).
#   SUBSTRATE_SERVICE         Substrate service name (default: knowledge-substrate).
#   GATEWAY_SERVICE           Gateway service name (default: knowledge-gateway).

set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" >/dev/null 2>&1 && pwd)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/.." >/dev/null 2>&1 && pwd)"

COMPOSE_FILE="${COMPOSE_FILE:-${REPO_ROOT}/deploy/docker-compose.yml}"
SUBSTRATE_SERVICE="${SUBSTRATE_SERVICE:-knowledge-substrate}"
GATEWAY_SERVICE="${GATEWAY_SERVICE:-knowledge-gateway}"

ENV_FILE=""
ASSUME_YES=0

usage() {
    sed -n '2,40p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
}

die() {
    echo "error: $*" >&2
    exit 1
}

is_hex64() {
    [[ "$1" =~ ^[0-9a-fA-F]{64}$ ]]
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --env-file)
            [[ $# -ge 2 ]] || die "--env-file requires a path argument"
            ENV_FILE="$2"
            shift 2
            ;;
        --env-file=*)
            ENV_FILE="${1#*=}"
            shift
            ;;
        --yes|-y)
            ASSUME_YES=1
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            die "unexpected argument: $1 (see --help)"
            ;;
    esac
done

# Default to the repo-root .env only if it exists (this is the file the
# Makefile's `docker compose -f deploy/docker-compose.yml` reads, since
# compose loads .env from the directory it is invoked in).
if [[ -z "${ENV_FILE}" && -f "${REPO_ROOT}/.env" ]]; then
    ENV_FILE="${REPO_ROOT}/.env"
fi

command -v docker >/dev/null 2>&1 || die "docker not found on PATH"
[[ -f "${COMPOSE_FILE}" ]] || die "compose file not found: ${COMPOSE_FILE}"

# Resolve the docker compose invocation (v2 plugin vs legacy binary).
if docker compose version >/dev/null 2>&1; then
    COMPOSE=(docker compose -f "${COMPOSE_FILE}")
elif command -v docker-compose >/dev/null 2>&1; then
    COMPOSE=(docker-compose -f "${COMPOSE_FILE}")
else
    die "neither 'docker compose' nor 'docker-compose' is available"
fi
if [[ -n "${ENV_FILE}" ]]; then
    COMPOSE+=(--env-file "${ENV_FILE}")
fi

: "${KNOWLEDGE_MASTER_KEY:?KNOWLEDGE_MASTER_KEY (old key) must be set}"
: "${KNOWLEDGE_NEW_MASTER_KEY:?KNOWLEDGE_NEW_MASTER_KEY (new key) must be set}"
is_hex64 "${KNOWLEDGE_MASTER_KEY}" || die "KNOWLEDGE_MASTER_KEY must be 64 hex characters"
is_hex64 "${KNOWLEDGE_NEW_MASTER_KEY}" || die "KNOWLEDGE_NEW_MASTER_KEY must be 64 hex characters"
[[ "${KNOWLEDGE_MASTER_KEY}" != "${KNOWLEDGE_NEW_MASTER_KEY}" ]] || die "old and new keys are identical"

echo "About to rotate the substrate master key."
echo "  compose file: ${COMPOSE_FILE}"
echo "  services:     ${SUBSTRATE_SERVICE} (+ ${GATEWAY_SERVICE} will be stopped)"
if [[ -n "${ENV_FILE}" ]]; then
    echo "  env file:     ${ENV_FILE} (KNOWLEDGE_MASTER_KEY will be rewritten on success)"
else
    echo "  env file:     none (you must update KNOWLEDGE_MASTER_KEY yourself afterwards)"
fi

if [[ "${ASSUME_YES}" -ne 1 ]]; then
    read -r -p "Proceed? The substrate will be stopped during rotation [y/N] " reply
    case "${reply}" in
        y|Y|yes|YES) ;;
        *) die "aborted by operator" ;;
    esac
fi

echo ">> stopping ${GATEWAY_SERVICE} and ${SUBSTRATE_SERVICE} ..."
"${COMPOSE[@]}" stop "${GATEWAY_SERVICE}" "${SUBSTRATE_SERVICE}"

echo ">> running knowledge-rotate-key against the substrate data volume ..."
# `--no-deps` keeps the one-off container from starting llama-server et
# al.; `--rm` discards it afterwards. The substrate service definition
# mounts `substrate-data:/data` and sets KNOWLEDGE_STORE_PATH, so the
# tool sees the same on-disk databases the server uses. The keys are
# forwarded from this script's environment.
"${COMPOSE[@]}" run --rm --no-deps \
    -e "KNOWLEDGE_MASTER_KEY=${KNOWLEDGE_MASTER_KEY}" \
    -e "KNOWLEDGE_NEW_MASTER_KEY=${KNOWLEDGE_NEW_MASTER_KEY}" \
    --entrypoint knowledge-rotate-key \
    "${SUBSTRATE_SERVICE}"

echo ">> rotation succeeded."

if [[ -n "${ENV_FILE}" && -w "${ENV_FILE}" ]]; then
    echo ">> updating KNOWLEDGE_MASTER_KEY in ${ENV_FILE} ..."
    tmp="$(mktemp)"
    if grep -q '^KNOWLEDGE_MASTER_KEY=' "${ENV_FILE}"; then
        sed "s|^KNOWLEDGE_MASTER_KEY=.*|KNOWLEDGE_MASTER_KEY=${KNOWLEDGE_NEW_MASTER_KEY}|" \
            "${ENV_FILE}" >"${tmp}"
    else
        cat "${ENV_FILE}" >"${tmp}"
        echo "KNOWLEDGE_MASTER_KEY=${KNOWLEDGE_NEW_MASTER_KEY}" >>"${tmp}"
    fi
    # Preserve the original file mode where possible.
    cat "${tmp}" >"${ENV_FILE}"
    rm -f "${tmp}"

    echo ">> restarting the stack under the new key ..."
    "${COMPOSE[@]}" up -d "${SUBSTRATE_SERVICE}" "${GATEWAY_SERVICE}"
    echo "done. Verify the substrate is healthy, then destroy the *.bak.* backups in the data volume."
else
    cat <<EOF
done. The on-disk stores are now encrypted under the NEW key, but the
stack is still STOPPED. To finish:

  1. Set KNOWLEDGE_MASTER_KEY to the new key wherever the deployment
     reads it (compose env file / secret manager).
  2. Start the stack:  docker compose up -d ${SUBSTRATE_SERVICE} ${GATEWAY_SERVICE}
  3. Confirm the substrate is healthy, then securely destroy the
     *.bak.* files in the substrate-data volume (they open under the
     OLD key).
EOF
fi
