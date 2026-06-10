#!/usr/bin/env bash
#
# Download the on-device model artifacts for local / on-device development
# with SHA-256 verification.
#
# You do NOT need this for the Docker / Compose / Helm deployment: the
# published `llama-server` image ships the Bonsai-1.7B GGUF baked in (see
# deploy/Dockerfile.llama-server). This script is for native local builds
# and on-device (iOS/macOS/Android) packaging.
#
# Artifacts (see deploy/model-artifacts/README.md):
#   - bonsai-1.7b.gguf            GGUF (server-side synthesis)
#   - bonsai-1.7b-mlx-2bit.tar.gz MLX 2-bit (Apple Silicon on-device)
#   - xlm-r-embed-int8.onnx       XLM-R embedding model (INT8)
#   - xlm-r-embed-int4.onnx       XLM-R embedding model (INT4)
#
# Each download is checked against deploy/model-artifacts/SHA256SUMS.
# Until a checksum is pinned there for a given release, the script prints
# the computed hash and continues; pass --require-checksums (or set
# REQUIRE_CHECKSUMS=1) to fail on an unpinned/mismatching artifact instead.
#
# Usage:
#   ./scripts/download-models.sh [--dest DIR] [--require-checksums] [--force]
#
# Environment overrides:
#   MODEL_DIR            destination directory (default: deploy/models)
#   REQUIRE_CHECKSUMS    set to 1 to require pinned, matching checksums
#   <NAME>_URL           override a single artifact URL, where <NAME> is the
#                        upper-cased filename with non-alnum chars as '_'
#                        (e.g. BONSAI_1_7B_GGUF_URL).

set -euo pipefail

# Resolve repo root from this script's location so it works from anywhere.
SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" >/dev/null 2>&1 && pwd)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/.." >/dev/null 2>&1 && pwd)"
SUMS_FILE="${REPO_ROOT}/deploy/model-artifacts/SHA256SUMS"

DEST="${MODEL_DIR:-${REPO_ROOT}/deploy/models}"
REQUIRE_CHECKSUMS="${REQUIRE_CHECKSUMS:-0}"
FORCE=0

# Artifact manifest: "filename|default_url". Keep filenames in sync with
# deploy/model-artifacts/SHA256SUMS and README.md.
ARTIFACTS=(
  "bonsai-1.7b.gguf|https://huggingface.co/prism-ml/Bonsai-1.7B-gguf/resolve/main/Bonsai-1.7B.gguf"
  "bonsai-1.7b-mlx-2bit.tar.gz|https://huggingface.co/kennguy3n/bonsai-1.7b-mlx/resolve/main/bonsai-1.7b-mlx-2bit.tar.gz"
  "xlm-r-embed-int8.onnx|https://huggingface.co/kennguy3n/xlm-r-embed-onnx/resolve/main/xlm-r-embed-int8.onnx"
  "xlm-r-embed-int4.onnx|https://huggingface.co/kennguy3n/xlm-r-embed-onnx/resolve/main/xlm-r-embed-int4.onnx"
)

usage() {
  # Print the leading doc-comment block: every line from the first doc
  # line (3) up to the first non-comment line, with the leading "# "
  # stripped. Deriving the end from the comment structure (rather than a
  # hard-coded line number) keeps --help correct as the header changes.
  awk 'NR < 3 { next } /^#/ { sub(/^# ?/, ""); print; next } { exit }' "${BASH_SOURCE[0]}"
}

log() { printf '==> %s\n' "$*"; }
warn() { printf 'WARN: %s\n' "$*" >&2; }
err() { printf 'ERROR: %s\n' "$*" >&2; exit 1; }

while [ "$#" -gt 0 ]; do
  case "$1" in
    --dest)
      [ "$#" -ge 2 ] || err "--dest requires a directory argument"
      DEST="$2"; shift 2 ;;
    --dest=*)
      DEST="${1#--dest=}"; shift ;;
    --require-checksums)
      REQUIRE_CHECKSUMS=1; shift ;;
    --force)
      FORCE=1; shift ;;
    -h|--help)
      usage; exit 0 ;;
    *)
      err "unknown argument: $1 (try --help)" ;;
  esac
done

# Pick a downloader once.
if command -v curl >/dev/null 2>&1; then
  download() { curl -fSL --retry 3 -o "$2" "$1"; }
elif command -v wget >/dev/null 2>&1; then
  download() { wget -q -O "$2" "$1"; }
else
  err "need curl or wget on PATH to download artifacts"
fi

# Compute a SHA-256 with whichever tool is available.
if command -v sha256sum >/dev/null 2>&1; then
  sha256_of() { sha256sum "$1" | awk '{print $1}'; }
elif command -v shasum >/dev/null 2>&1; then
  sha256_of() { shasum -a 256 "$1" | awk '{print $1}'; }
else
  err "need sha256sum or shasum on PATH to verify artifacts"
fi

# Look up the pinned checksum for a filename from SHA256SUMS (ignoring
# comments/blank lines). Prints the hash, or nothing if unpinned.
expected_sum() {
  local name="$1"
  [ -f "$SUMS_FILE" ] || return 0
  awk -v f="$name" '
    /^[[:space:]]*#/ { next }
    /^[[:space:]]*$/ { next }
    { sum=$1; $1=""; sub(/^[[:space:]]+/, ""); if ($0 == f) { print sum; exit } }
  ' "$SUMS_FILE"
}

# Derive the per-artifact URL override env var name from a filename.
url_override_var() {
  printf '%s' "$1" | tr '[:lower:]' '[:upper:]' | tr -c 'A-Z0-9' '_' | sed 's/_*$//'
}

mkdir -p "$DEST"
log "Destination: $DEST"

failures=0
for entry in "${ARTIFACTS[@]}"; do
  name="${entry%%|*}"
  default_url="${entry#*|}"

  var="$(url_override_var "$name")_URL"
  url="${!var:-$default_url}"
  out="$DEST/$name"

  if [ -f "$out" ] && [ "$FORCE" -eq 0 ]; then
    log "$name already present (use --force to re-download)"
  else
    log "Downloading $name"
    log "  from $url"
    tmp="$out.partial"
    if ! download "$url" "$tmp"; then
      warn "failed to download $name from $url"
      rm -f "$tmp"
      failures=$((failures + 1))
      continue
    fi
    mv -f "$tmp" "$out"
  fi

  actual="$(sha256_of "$out")"
  expected="$(expected_sum "$name")"
  if [ -z "$expected" ]; then
    warn "no pinned checksum for $name; computed sha256: $actual"
    if [ "$REQUIRE_CHECKSUMS" -eq 1 ]; then
      warn "  (--require-checksums set: treating unpinned artifact as a failure)"
      failures=$((failures + 1))
    fi
  elif [ "$actual" = "$expected" ]; then
    log "$name checksum OK ($actual)"
  else
    warn "$name checksum MISMATCH"
    warn "  expected: $expected"
    warn "  actual:   $actual"
    # A verified-wrong artifact is untrustworthy: remove it so it is never
    # consumed and a plain re-run re-downloads it (instead of skipping the
    # download because the corrupt file is still present).
    rm -f "$out"
    failures=$((failures + 1))
  fi
done

if [ "$failures" -gt 0 ]; then
  err "$failures artifact(s) failed to download or verify"
fi

log "All artifacts downloaded and verified into $DEST"
