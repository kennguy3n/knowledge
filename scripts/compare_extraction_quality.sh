#!/usr/bin/env bash
#
# Dual-run extraction-quality comparison.
#
# Runs the observation-extraction quality eval twice — a "baseline"
# configuration and a "candidate" configuration — parses the per-type
# F1 scores (and the macro-F1) printed by each run, and diffs them.
# Exits non-zero if the candidate regresses any metric past the
# tolerance, so this can gate CI the same way scripts/compare_benchmarks.py
# gates the Criterion baselines.
#
# Both runs default to the exact invocation CI uses for the
# extraction-quality gate:
#
#     cargo test -p integration_tests --test observation_eval -- --nocapture
#
# which prints an "Observation Eval Report" whose per-type rows look like
#
#     decision    P=0.900 R=0.857 F1=0.878 (tp=6 fp=1 fn=1)
#     ...
#     macro-F1 = 0.640
#
# Usage:
#   ./scripts/compare_extraction_quality.sh
#
# Environment overrides (all optional):
#   BASELINE_CMD   command for the baseline run (default: the CI eval).
#   CANDIDATE_CMD  command for the candidate run (default: same as baseline).
#   F1_TOLERANCE   absolute F1 drop tolerated before a metric is a
#                  regression (default: 0.01).
#
# Exit codes:
#   0  candidate is within tolerance of the baseline on every metric.
#   1  at least one metric regressed past the tolerance.
#   2  usage / IO error (an eval run failed or printed no parsable report).

set -euo pipefail

# Resolve repo root from this script's location so it works from anywhere.
SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" >/dev/null 2>&1 && pwd)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/.." >/dev/null 2>&1 && pwd)"
cd "${REPO_ROOT}"

DEFAULT_CMD="cargo test -p integration_tests --test observation_eval -- --nocapture"
BASELINE_CMD="${BASELINE_CMD:-${DEFAULT_CMD}}"
CANDIDATE_CMD="${CANDIDATE_CMD:-${BASELINE_CMD}}"
F1_TOLERANCE="${F1_TOLERANCE:-0.01}"

WORKDIR="$(mktemp -d)"
trap 'rm -rf "${WORKDIR}"' EXIT

# run_eval <label> <command> <metrics-out>
#
# Executes the eval command, echoing its output for CI visibility, and
# writes parsed `<metric> <f1>` pairs (one per line) to <metrics-out>.
# Per-type rows and the macro-F1 summary are both captured. The report
# is printed once per eval test, so we keep only the first value seen
# for each metric (the runs are deterministic — identical reports).
run_eval() {
  local label="$1" cmd="$2" out="$3"
  local raw="${WORKDIR}/${label}.log"

  echo "── ${label} run: ${cmd}"
  # Capture stdout+stderr: the report is printed via both println! and
  # eprintln! depending on which eval test emits it.
  if ! eval "${cmd}" >"${raw}" 2>&1; then
    cat "${raw}" >&2
    echo "error: ${label} eval run failed (command exited non-zero)" >&2
    exit 2
  fi
  cat "${raw}"

  # Per-type rows: "  <type>   P=.. R=.. F1=<f1> (..)".
  sed -nE 's/^[[:space:]]*([a-z]+)[[:space:]]+P=[0-9.]+ R=[0-9.]+ F1=([0-9.]+).*/\1 \2/p' \
    "${raw}" >"${out}.dup" || true
  # Macro-F1 summary line: "  macro-F1 = <f1>".
  sed -nE 's/^[[:space:]]*macro-F1[[:space:]]*=[[:space:]]*([0-9.]+).*/macro-F1 \1/p' \
    "${raw}" >>"${out}.dup" || true

  # Keep the first F1 seen per metric (reports repeat across eval tests).
  awk '!seen[$1]++' "${out}.dup" >"${out}"
  rm -f "${out}.dup"

  if [ ! -s "${out}" ]; then
    echo "error: ${label} run produced no parsable F1 metrics" >&2
    exit 2
  fi
}

BASELINE_METRICS="${WORKDIR}/baseline.metrics"
CANDIDATE_METRICS="${WORKDIR}/candidate.metrics"

run_eval "baseline" "${BASELINE_CMD}" "${BASELINE_METRICS}"
run_eval "candidate" "${CANDIDATE_CMD}" "${CANDIDATE_METRICS}"

echo
echo "Comparing candidate vs baseline extraction quality (F1 tolerance ${F1_TOLERANCE}):"
echo

# Diff the two metric sets. A metric regresses when the candidate F1 is
# more than F1_TOLERANCE below the baseline F1. A metric present in the
# baseline but missing from the candidate is also a regression (the type
# stopped being detected at all).
regressions="$(
  awk -v tol="${F1_TOLERANCE}" '
    FNR==NR { base[$1]=$2; next }
    { cand[$1]=$2 }
    END {
      n = 0
      for (m in base) {
        b = base[m]
        if (m in cand) {
          c = cand[m]
          delta = c - b
          status = (delta < -tol) ? "REGRESSED" : "ok"
          printf "  [%9s] %-10s baseline F1=%.3f candidate F1=%.3f (%+.3f)\n", status, m, b, c, delta > "/dev/stderr"
          if (delta < -tol) { print m; n++ }
        } else {
          printf "  [%9s] %-10s baseline F1=%.3f candidate F1=MISSING\n", "REGRESSED", m, b > "/dev/stderr"
          print m; n++
        }
      }
    }
  ' "${BASELINE_METRICS}" "${CANDIDATE_METRICS}"
)"

echo
if [ -n "${regressions}" ]; then
  count="$(printf '%s\n' "${regressions}" | grep -c .)"
  echo "FAIL: ${count} metric(s) regressed past ${F1_TOLERANCE}:"
  while IFS= read -r metric; do
    [ -n "${metric}" ] && echo "  - ${metric}"
  done <<<"${regressions}"
  exit 1
fi

echo "PASS: candidate extraction quality is within tolerance of the baseline."
