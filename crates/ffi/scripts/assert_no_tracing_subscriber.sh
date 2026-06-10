#!/usr/bin/env bash
#
# Fails if any of the supplied compiled artefacts link against the
# `tracing-subscriber` crate.
#
# Why this exists
# ---------------
# `crates/ffi/Cargo.toml`'s `tracing-subscriber` feature is OFF by
# default and MUST stay off in production mobile builds: the tracing
# infrastructure has measurable per-event overhead (every
# `tracing::info!` / `tracing::debug!` call site in the substrate pays
# a callsite check even with no subscriber installed, and once a
# subscriber is installed every event is formatted and dispatched).
# This guard turns "the feature is off" from a convention into a
# build-time invariant: it scans the symbol table of the produced
# static / shared library and fails if any `tracing_subscriber`
# symbol is present.
#
# It is empirically reliable: a release `ffi` artefact built WITHOUT
# the feature contains zero `tracing_subscriber` symbols, while the
# same artefact built WITH `--features tracing-subscriber` contains
# hundreds. Rust's mangled symbol names embed the originating crate
# name (`tracing_subscriber`) verbatim, so a plain substring match on
# `nm` output is sufficient and toolchain-agnostic.
#
# Usage
# -----
#   assert_no_tracing_subscriber.sh <nm-binary> <artefact> [<artefact> ...]
#
# `<nm-binary>` is the symbol dumper appropriate for the artefact's
# target — the host `nm` for a host build, `llvm-nm` (from the Android
# NDK or an LLVM install) for an `aarch64-linux-android` `.so`, or the
# Xcode `nm` for an `aarch64-apple-ios` `.a`.
#
# Exit status: 0 if every artefact is clean, 1 if any artefact links
# `tracing-subscriber` (or on a usage / IO error).

set -euo pipefail

# The crate name as it appears, verbatim, inside Rust's mangled
# symbols. Underscored form because that is how the crate identifier
# is spelled in symbol names (the hyphenated `tracing-subscriber` is
# only the Cargo package name).
readonly NEEDLE="tracing_subscriber"

if [ "$#" -lt 2 ]; then
  echo "usage: $0 <nm-binary> <artefact> [<artefact> ...]" >&2
  exit 1
fi

NM_BIN="$1"
shift

if ! command -v "${NM_BIN}" >/dev/null 2>&1; then
  echo "error: symbol dumper '${NM_BIN}' not found on PATH" >&2
  exit 1
fi

status=0
for artefact in "$@"; do
  if [ ! -f "${artefact}" ]; then
    echo "error: artefact '${artefact}' does not exist" >&2
    status=1
    continue
  fi

  # `grep -c` exits non-zero when the count is 0; that is the success
  # case here, so swallow its status and read the count instead.
  count="$("${NM_BIN}" "${artefact}" 2>/dev/null | grep -c "${NEEDLE}" || true)"

  if [ "${count}" -ne 0 ]; then
    echo "FAIL: '${artefact}' links tracing-subscriber (${count} matching symbols)." >&2
    echo "      The tracing-subscriber feature must never be enabled in a" >&2
    echo "      production mobile build — see crates/ffi/Cargo.toml." >&2
    # Surface a few offending symbols to make the failure actionable.
    "${NM_BIN}" "${artefact}" 2>/dev/null | grep "${NEEDLE}" | head -5 >&2 || true
    status=1
  else
    echo "ok: '${artefact}' is free of tracing-subscriber symbols."
  fi
done

exit "${status}"
