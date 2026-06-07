#!/usr/bin/env python3
"""Compare Criterion benchmark results against the documented baselines.

Reads ``docs/operator/perf-baselines.json`` (the machine-readable mirror
of the cost-model performance table) and, for each metric, loads the
matching Criterion ``estimates.json`` under ``target/criterion/`` and
checks that the freshly-measured value has not regressed past the
configured tolerance.

* latency metrics (``lower_is_better``) regress when the measured time
  rises above ``baseline * (1 + tolerance)``;
* throughput metrics (``higher_is_better``) regress when measured
  throughput falls below ``baseline * (1 - tolerance)``.

Criterion's ``estimates.json`` reports the per-iteration time in
nanoseconds (``mean.point_estimate``). For throughput metrics the
per-iteration time covers ``element_count`` elements, so
``throughput = element_count / (time_ns / 1e9)``.

Usage:
    compare_benchmarks.py [--baselines PATH] [--criterion-dir DIR]

Exit code 0 if all metrics are within tolerance, 1 on any regression,
2 on a usage/IO error (e.g. a referenced estimates.json is missing —
that means the bench did not run and must be treated as a hard failure).
"""
from __future__ import annotations

import argparse
import json
import os
import sys


def load_mean_ns(criterion_dir: str, criterion_path: str) -> float:
    """Return the mean per-iteration time (ns) for a Criterion bench."""
    estimates = os.path.join(criterion_dir, criterion_path, "new", "estimates.json")
    if not os.path.exists(estimates):
        raise FileNotFoundError(
            f"missing Criterion estimates for {criterion_path!r} at {estimates} "
            "(did the benchmark run?)"
        )
    with open(estimates, encoding="utf-8") as fh:
        doc = json.load(fh)
    return float(doc["mean"]["point_estimate"])


def measured_value(metric: dict, mean_ns: float) -> float:
    """Convert the raw mean-ns into the metric's reported unit."""
    if metric["kind"] == "latency":
        return mean_ns / 1e6  # ns -> ms
    if metric["kind"] == "throughput":
        seconds = mean_ns / 1e9
        return metric["element_count"] / seconds  # elements / sec
    raise ValueError(f"unknown metric kind {metric['kind']!r}")


def is_regression(metric: dict, measured: float, tol: float) -> bool:
    baseline = metric["baseline"]
    if metric["direction"] == "lower_is_better":
        return measured > baseline * (1.0 + tol / 100.0)
    if metric["direction"] == "higher_is_better":
        return measured < baseline * (1.0 - tol / 100.0)
    raise ValueError(f"unknown direction {metric['direction']!r}")


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baselines", default="docs/operator/perf-baselines.json")
    parser.add_argument("--criterion-dir", default="target/criterion")
    args = parser.parse_args(argv[1:])

    with open(args.baselines, encoding="utf-8") as fh:
        cfg = json.load(fh)
    tol = float(cfg["tolerance_pct"])

    regressions: list[str] = []
    print(f"Comparing benchmarks against baselines (tolerance ±{tol:.0f}%):\n")
    for metric in cfg["metrics"]:
        mean_ns = load_mean_ns(args.criterion_dir, metric["criterion_path"])
        measured = measured_value(metric, mean_ns)
        baseline = metric["baseline"]
        delta_pct = (measured - baseline) / baseline * 100.0
        regressed = is_regression(metric, measured, tol)
        status = "REGRESSED" if regressed else "ok"
        print(
            f"  [{status:>9}] {metric['name']}: measured {measured:.2f} "
            f"{metric['unit']} vs baseline {baseline:.2f} {metric['unit']} "
            f"({delta_pct:+.1f}%)"
        )
        if regressed:
            regressions.append(metric["name"])

    print()
    if regressions:
        print(f"FAIL: {len(regressions)} metric(s) regressed past ±{tol:.0f}%: {regressions}")
        return 1
    print("PASS: all metrics within tolerance of the documented baselines.")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main(sys.argv))
    except (FileNotFoundError, KeyError, ValueError) as exc:
        print(f"error: {exc}", file=sys.stderr)
        raise SystemExit(2)
