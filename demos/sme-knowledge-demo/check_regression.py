#!/usr/bin/env python3
"""Regression guard for the SME demo assertion set.

Compares a freshly-produced ``sme_demo_results.json`` against the
committed baseline and fails (exit 1) if any assertion that *passed* in
the baseline is missing or now failing in the fresh run. New assertions
and newly-passing assertions are allowed — the guard is one-directional
(it only forbids regressions), so the demo can grow without churn here.

Usage:
    check_regression.py <baseline.json> <fresh.json>

Used by the ``connector_audit`` CI job (.github/workflows/ci.yml) after
running ``run_demo.py`` against the dockerized gateway.
"""
from __future__ import annotations

import json
import sys


def load_passing(path: str) -> dict[str, bool]:
    """Return {assertion_name: passed} for every step in *path*."""
    with open(path, encoding="utf-8") as fh:
        doc = json.load(fh)
    steps = doc.get("steps", [])
    result: dict[str, bool] = {}
    for step in steps:
        name = step["assertion"]
        # A duplicate assertion name regressing in either occurrence is
        # a regression, so AND the outcomes rather than letting a later
        # pass mask an earlier fail.
        result[name] = result.get(name, True) and bool(step["passed"])
    return result


def main(argv: list[str]) -> int:
    if len(argv) != 3:
        print(f"usage: {argv[0]} <baseline.json> <fresh.json>", file=sys.stderr)
        return 2
    baseline = load_passing(argv[1])
    fresh = load_passing(argv[2])

    regressions: list[str] = []
    for name, passed in baseline.items():
        if not passed:
            continue  # only previously-passing assertions are guarded
        if name not in fresh:
            regressions.append(f"  - MISSING: {name!r} (was passing, no longer present)")
        elif not fresh[name]:
            regressions.append(f"  - FAILING: {name!r} (was passing, now fails)")

    if regressions:
        print("SME demo regression guard FAILED — previously-passing assertions regressed:")
        print("\n".join(regressions))
        return 1

    new = sorted(set(fresh) - set(baseline))
    print(
        f"SME demo regression guard OK: {len(baseline)} baseline assertions all "
        f"still passing ({len(new)} new assertion(s) added)."
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
