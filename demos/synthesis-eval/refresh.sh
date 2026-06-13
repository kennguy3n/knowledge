#!/usr/bin/env bash
#
# Refresh + verify the whole offline synthesis-eval surface in one command:
#   - docs/technical/synthesis-eval.md          (A1 per-recap eval)
#   - docs/technical/multilingual-leaderboard.md (C4 per-language leaderboard)
#   - demos/synthesis-eval/leaderboard_snapshot.json
#
# It regenerates the committed artifacts, then runs every gate (the synthesis
# regression gate, the leaderboard determinism gate, and the unit/regression
# tests). No model, network or GPU is involved — everything scores the
# already-recorded demo output, so this is safe to run anywhere, including CI.
#
# Usage:
#   demos/synthesis-eval/refresh.sh            # regenerate, then verify
#   demos/synthesis-eval/refresh.sh --check    # verify only (no regeneration)
set -euo pipefail

cd "$(dirname "$0")"

# Parse the single optional mode flag explicitly so a typo (e.g. `--chek`)
# fails loud instead of silently falling through to the regenerate path.
regenerate=1
case "${1:-}" in
  "")        ;;                 # default: regenerate, then verify
  --check)   regenerate=0 ;;    # verify only (no regeneration)
  *)
    echo "refresh.sh: unknown argument '$1'" >&2
    echo "usage: refresh.sh [--check]" >&2
    exit 2
    ;;
esac

if (( regenerate )); then
  echo "==> regenerating committed artifacts"
  python3 run_eval.py
  python3 leaderboard.py
fi

echo "==> synthesis-eval regression gate"
python3 run_eval.py --check

echo "==> multilingual-leaderboard determinism gate"
python3 leaderboard.py --check

echo "==> unit + regression tests"
python3 -m unittest discover -s . -p 'test_*.py'

echo "==> OK"
