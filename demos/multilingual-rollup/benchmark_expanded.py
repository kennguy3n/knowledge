#!/usr/bin/env python3
"""
Expanded live benchmark: 48 sessions (3 scenarios × 16 languages) × 6 messages × 8 expected terms.

Drives synthesis prompts directly against llama-server instances and scores
output with the same deterministic scorers used by the synthesis-eval harness.

Usage:
    # Start llama-servers first:
    #   llama-server --model qwen3.5-0.8b-q4_k_m.gguf --port 8083 -c 4096 --jinja
    #   llama-server --model qwen3.5-2b-q4_k_m.gguf   --port 8084 -c 4096 --jinja
    #
    LLAMA_08B_URL=http://127.0.0.1:8083 \
    LLAMA_2B_URL=http://127.0.0.1:8084 \
    python3 demos/multilingual-rollup/benchmark_expanded.py

Outputs:
    results/expanded_benchmark.json  — machine-readable results
    results/expanded_benchmark.md    — human-readable comparison report
"""

from __future__ import annotations

import json
import os
import sys
import time
import urllib.request
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parent.parent
RESULTS_DIR = HERE / "results"
DATASET_PATH = HERE / "dataset" / "expanded-benchmark.json"
FIXTURE_PATH = HERE / "fixtures" / "expanded-expected-terms.json"

# Re-use the eval harness scorers
sys.path.insert(0, str(REPO / "demos" / "synthesis-eval"))
import scorers

# Re-use the grammar + llama probe from run_rollup
sys.path.insert(0, str(HERE))
from run_rollup import GRAMMAR, SAMPLING, _llama, quality_report, in_language, script_of

LLAMA_08B = os.environ.get("LLAMA_08B_URL", "http://127.0.0.1:8083").rstrip("/")
LLAMA_2B = os.environ.get("LLAMA_2B_URL", "http://127.0.0.1:8084").rstrip("/")

N_PREDICT = 800  # longer sessions need more tokens

# Per-class prompt variants — mirror the production Rust constants in
# crates/inference_router/src/task.rs (PROMPT_SYNTH_SUMMARY_SMALL / _MEDIUM).
# The 0.8B model is ModelClass::Small; the 2B model is ModelClass::Medium.
SYNTH_PROMPT_SMALL = (
    'Output ONLY the JSON object. '
    'CRITICAL: The very first characters must be {"recap":" — do NOT start with '
    '\'"The session\", \"This summary\", \"The following\", or any description of the task. '
    'Do not preface, explain, or describe the output.\n'
    'Summarise the session as a JSON object with this exact shape: '
    '{"recap": "…", "decisions": ["…"], "open_questions": ["…"], "active_tasks": ["…"]}. '
    'The recap is a 3-5 sentence factual headline that includes ALL specific identifiers '
    '(person names, SKU codes, invoice numbers, lot IDs, monetary amounts, dates, and technical terms) '
    'mentioned in the session. '
    'The recap MUST be written in the same language and script as the session messages — '
    'if the session is in French, write in French; if in Japanese, write in Japanese; if in Arabic, '
    'write in Arabic. Do not translate to English. '
    'The other fields each list zero or more strings in the session\'s language.\n'
    'The example below shows only the JSON shape — its placeholder tokens are NOT '
    'content: always write the values from the session itself, in the session\'s own '
    'language, never copy the example\'s tokens.\n\n'
    'Example session (format illustration only):\n'
    'Observations:\n'
    '- [decision] (important) EXAMPLE_DECISION\n'
    '- [task] (important) EXAMPLE_TASK\n'
    'Example output:\n'
    '{"recap":"EXAMPLE_DECISION was agreed and EXAMPLE_TASK was scheduled.",'
    '"decisions":["EXAMPLE_DECISION"],'
    '"open_questions":[],"active_tasks":["EXAMPLE_TASK"]}\n\n'
    'Session:\n{body}'
)

SYNTH_PROMPT_MEDIUM = (
    'Output ONLY the JSON object. Do not describe the task, do not preface or '
    'explain the output, and do not write about \"the session\" or \"this summary\". '
    'Summarise the session as a JSON object with this exact shape: '
    '{"recap": "…", "decisions": ["…"], "open_questions": ["…"], "active_tasks": ["…"]}. '
    'The recap is a 2-4 sentence factual headline that includes specific identifiers '
    '(person names, SKU codes, invoice numbers, lot IDs, monetary amounts, dates, and '
    'technical terms) mentioned in the session. '
    'The recap is written in the same language as the session; the other fields each '
    'list zero or more strings. '
    'The example below shows only the JSON shape — its placeholder tokens are NOT '
    'content: always write the values from the session itself, in the session\'s own '
    'language, never copy the example\'s tokens.\n\n'
    'Example session (format illustration only):\n'
    'Observations:\n'
    '- [decision] (important) EXAMPLE_DECISION\n'
    '- [task] (important) EXAMPLE_TASK\n'
    'Example output:\n'
    '{"recap":"EXAMPLE_DECISION was agreed and EXAMPLE_TASK was scheduled.",'
    '"decisions":["EXAMPLE_DECISION"],'
    '"open_questions":[],"active_tasks":["EXAMPLE_TASK"]}\n\n'
    'Session:\n{body}'
)

# Map model URL → per-class prompt
PROMPT_FOR_MODEL = {
    LLAMA_08B: SYNTH_PROMPT_SMALL,
    LLAMA_2B: SYNTH_PROMPT_MEDIUM,
}


def benchmark_session(base_url: str, messages: list[str]) -> dict:
    """Send a session's messages to llama-server and get back a recap."""
    session = "\n".join(f"- {m}" for m in messages)
    prompt = PROMPT_FOR_MODEL.get(base_url, SYNTH_PROMPT_MEDIUM)
    t0 = time.time()
    content = _llama(base_url, prompt.replace("{body}", session), n_predict=N_PREDICT)
    latency_ms = round((time.time() - t0) * 1000, 1)
    try:
        parsed = json.loads(content)
        recap = parsed.get("recap", "")
    except Exception:
        recap = content.strip()[:500]
    return {"recap": recap, "recap_chars": len(recap), "latency_ms": latency_ms}


def score_session(row: dict, expected_terms: list[str], lang: str, model_name: str) -> dict:
    """Score a single session result."""
    recap = row.get("recap", "")
    cov = scorers.term_coverage(recap, expected_terms)
    in_lang = scorers.in_language(lang, recap)
    quality = quality_report(recap)
    return {
        "language": lang,
        "model": model_name,
        "coverage": {
            "matched": len(cov.matched),
            "expected": len(cov.expected),
            "fraction": round(cov.fraction, 4),
            "missing": cov.missing,
        },
        "in_language": in_lang,
        "usable": quality.get("usable", False),
        "latency_ms": row.get("latency_ms", 0),
        "recap_chars": row.get("recap_chars", 0),
        "recap": recap[:300],
    }


def benchmark_model(base_url: str, dataset: dict, fixtures: dict, model_name: str) -> list[dict]:
    """Run all sessions against a single model."""
    results = []
    sessions = dataset["scenarios"]
    total = len(sessions)
    for i, sc in enumerate(sessions, 1):
        sid = sc["id"]
        lang = sc["language"]
        messages = sc["messages"]
        expected = fixtures["sessions"][sid]["expected_terms"]
        print(f"  [{i}/{total}] {sid}...", end=" ", flush=True)
        try:
            row = benchmark_session(base_url, messages)
            scored = score_session(row, expected, lang, model_name)
            scored["session_id"] = sid
            scored["domain"] = sc["domain"]
            scored["script"] = sc["script"]
            results.append(scored)
            print(f"cov={scored['coverage']['matched']}/{scored['coverage']['expected']} "
                  f"in_lang={'Y' if scored['in_language'] else 'N'} "
                  f"usable={'Y' if scored['usable'] else 'N'} "
                  f"{scored['latency_ms']:.0f}ms")
        except Exception as exc:
            results.append({
                "session_id": sid,
                "language": lang,
                "model": model_name,
                "domain": sc["domain"],
                "script": sc["script"],
                "error": str(exc),
                "coverage": None,
                "in_language": False,
                "usable": False,
                "latency_ms": 0,
                "recap": "",
            })
            print(f"ERROR: {exc}")
    return results


def compute_stats(rows: list[dict]) -> dict:
    """Compute aggregate statistics."""
    n = len(rows)
    if n == 0:
        return {"n": 0}
    valid = [r for r in rows if "error" not in r]
    nv = len(valid)
    in_lang = sum(1 for r in valid if r.get("in_language"))
    usable = sum(1 for r in valid if r.get("usable"))
    covs = [r["coverage"]["fraction"] for r in valid if r.get("coverage")]
    avg_cov = sum(covs) / len(covs) if covs else 0
    med_cov = sorted(covs)[len(covs)//2] if covs else 0
    lats = [r["latency_ms"] for r in valid if r.get("latency_ms", 0) > 0]
    avg_lat = sum(lats) / len(lats) if lats else 0
    med_lat = sorted(lats)[len(lats)//2] if lats else 0
    chars = [r.get("recap_chars", 0) for r in valid]
    avg_chars = sum(chars) / len(chars) if chars else 0
    # Per-domain breakdown
    domains = {}
    for r in valid:
        d = r.get("domain", "unknown")
        if d not in domains:
            domains[d] = {"n": 0, "in_lang": 0, "usable": 0, "covs": []}
        domains[d]["n"] += 1
        if r.get("in_language"):
            domains[d]["in_lang"] += 1
        if r.get("usable"):
            domains[d]["usable"] += 1
        if r.get("coverage"):
            domains[d]["covs"].append(r["coverage"]["fraction"])
    for d in domains:
        dc = domains[d]["covs"]
        domains[d]["avg_cov"] = sum(dc)/len(dc) if dc else 0
    return {
        "n": n, "valid": nv, "in_language": in_lang, "usable": usable,
        "avg_coverage": round(avg_cov, 4), "median_coverage": round(med_cov, 4),
        "avg_latency_ms": round(avg_lat, 1), "median_latency_ms": round(med_lat, 1),
        "avg_recap_chars": round(avg_chars, 1),
        "in_language_pct": round(in_lang/nv*100, 1) if nv else 0,
        "usable_pct": round(usable/nv*100, 1) if nv else 0,
        "by_domain": domains,
    }


def build_report(qwen08b: list[dict], qwen2b: list[dict]) -> str:
    """Build a comprehensive comparison report."""
    lines = []
    a = lines.append

    a("# Expanded Benchmark: Qwen3.5-0.8B vs Qwen3.5-2B")
    a("")
    a(f"> 48 sessions (3 scenarios × 16 languages × 6 messages × 8 expected terms)")
    a(f"> Generated by `demos/multilingual-rollup/benchmark_expanded.py`")
    a("")

    s08 = compute_stats(qwen08b)
    s2 = compute_stats(qwen2b)

    # Summary table
    a("## Summary Statistics")
    a("")
    a("| Metric | Qwen3.5-0.8B | Qwen3.5-2B |")
    a("|--------|-------------|------------|")
    a(f"| Sessions | {s08['n']} | {s2['n']} |")
    a(f"| Valid (no error) | {s08['valid']} | {s2['valid']} |")
    a(f"| In-language | {s08['in_language']}/{s08['valid']} ({s08['in_language_pct']}%) | {s2['in_language']}/{s2['valid']} ({s2['in_language_pct']}%) |")
    a(f"| Usable | {s08['usable']}/{s08['valid']} ({s08['usable_pct']}%) | {s2['usable']}/{s2['valid']} ({s2['usable_pct']}%) |")
    a(f"| Avg term coverage | {s08['avg_coverage']*100:.1f}% | {s2['avg_coverage']*100:.1f}% |")
    a(f"| Median term coverage | {s08['median_coverage']*100:.1f}% | {s2['median_coverage']*100:.1f}% |")
    a(f"| Avg latency (ms) | {s08['avg_latency_ms']:.0f} | {s2['avg_latency_ms']:.0f} |")
    a(f"| Median latency (ms) | {s08['median_latency_ms']:.0f} | {s2['median_latency_ms']:.0f} |")
    a(f"| Avg recap length (chars) | {s08['avg_recap_chars']:.0f} | {s2['avg_recap_chars']:.0f} |")
    a("")

    # Per-domain breakdown
    a("## Per-Domain Breakdown")
    a("")
    all_domains = sorted(set(list(s08.get("by_domain", {}).keys()) + list(s2.get("by_domain", {}).keys())))
    a("| Domain | Model | N | In-lang | Usable | Avg coverage |")
    a("|--------|-------|---|---------|--------|--------------|")
    for d in all_domains:
        for label, stats in [("0.8B", s08), ("2B", s2)]:
            ds = stats.get("by_domain", {}).get(d, {})
            if ds:
                a(f"| {d} | {label} | {ds['n']} | {ds['in_lang']}/{ds['n']} | {ds['usable']}/{ds['n']} | {ds['avg_cov']*100:.1f}% |")
    a("")

    # Per-language × per-scenario detail
    a("## Per-Session Results")
    a("")
    a("| Session | Lang | Script | Domain | Model | Cov | In-lang | Usable | Latency |")
    a("|---------|------|--------|--------|-------|-----|---------|--------|---------|")
    all_sessions = sorted(set(
        [(r["session_id"], r.get("script",""), r.get("domain",""), r["language"]) for r in qwen08b + qwen2b]
    ), key=lambda x: (x[3], x[2]))
    q08_by_sid = {r["session_id"]: r for r in qwen08b}
    q2_by_sid = {r["session_id"]: r for r in qwen2b}
    for sid, script, domain, lang in all_sessions:
        for label, by_sid in [("0.8B", q08_by_sid), ("2B", q2_by_sid)]:
            r = by_sid.get(sid, {})
            cov = r.get("coverage")
            cov_s = f"{cov['matched']}/{cov['expected']}" if cov else "ERR"
            inl = "Y" if r.get("in_language") else "N"
            use = "Y" if r.get("usable") else "N"
            lat = f"{r.get('latency_ms',0):.0f}" if r.get("latency_ms",0) > 0 else "—"
            a(f"| {sid} | {lang} | {script} | {domain} | {label} | {cov_s} | {inl} | {use} | {lat} |")
    a("")

    # Coverage distribution histogram
    a("## Coverage Distribution")
    a("")
    for label, rows in [("Qwen3.5-0.8B", qwen08b), ("Qwen3.5-2B", qwen2b)]:
        covs = [r["coverage"]["fraction"] for r in rows if r.get("coverage")]
        if not covs:
            continue
        buckets = {"0%": 0, "1-25%": 0, "26-50%": 0, "51-75%": 0, "76-100%": 0}
        for c in covs:
            if c == 0:
                buckets["0%"] += 1
            elif c <= 0.25:
                buckets["1-25%"] += 1
            elif c <= 0.50:
                buckets["26-50%"] += 1
            elif c <= 0.75:
                buckets["51-75%"] += 1
            else:
                buckets["76-100%"] += 1
        a(f"### {label}")
        a("")
        a("| Range | Count | Bar |")
        a("|-------|-------|-----|")
        for rng, cnt in buckets.items():
            bar = "█" * cnt
            a(f"| {rng} | {cnt} | {bar} |")
        a("")

    # Sample recaps
    a("## Sample Recaps (first 300 chars)")
    a("")
    sample_sids = [sid for sid, _, _, _ in all_sessions[:6]]
    for sid in sample_sids:
        a(f"### {sid}")
        a("")
        for label, by_sid in [("Qwen3.5-0.8B", q08_by_sid), ("Qwen3.5-2B", q2_by_sid)]:
            r = by_sid.get(sid, {})
            recap = r.get("recap", "—")
            a(f"**{label}**: {recap}")
            a("")
        a("")

    return "\n".join(lines)


def main():
    if not DATASET_PATH.exists():
        print(f"ERROR: Dataset not found at {DATASET_PATH}", file=sys.stderr)
        print("Run `python3 demos/multilingual-rollup/gen_expanded.py` first.", file=sys.stderr)
        return 1
    if not FIXTURE_PATH.exists():
        print(f"ERROR: Fixtures not found at {FIXTURE_PATH}", file=sys.stderr)
        print("Run `python3 demos/multilingual-rollup/gen_expanded.py` first.", file=sys.stderr)
        return 1

    dataset = json.loads(DATASET_PATH.read_text(encoding="utf-8"))
    fixtures = json.loads(FIXTURE_PATH.read_text(encoding="utf-8"))

    qwen08b_results = []
    qwen2b_results = []

    # Benchmark Qwen3.5-0.8B
    print(f"\n=== Benchmarking Qwen3.5-0.8B at {LLAMA_08B} ===")
    try:
        urllib.request.urlopen(LLAMA_08B + "/health", timeout=10).read()
        qwen08b_results = benchmark_model(LLAMA_08B, dataset, fixtures, "Qwen3.5-0.8B Q4_K_M")
        print(f"  Completed: {len(qwen08b_results)} sessions")
    except Exception as e:
        print(f"  SKIP: {e}")

    # Benchmark Qwen3.5-2B
    print(f"\n=== Benchmarking Qwen3.5-2B at {LLAMA_2B} ===")
    try:
        urllib.request.urlopen(LLAMA_2B + "/health", timeout=10).read()
        qwen2b_results = benchmark_model(LLAMA_2B, dataset, fixtures, "Qwen3.5-2B Q4_K_M")
        print(f"  Completed: {len(qwen2b_results)} sessions")
    except Exception as e:
        print(f"  SKIP: {e}")

    if not qwen08b_results and not qwen2b_results:
        print("\nERROR: No results generated. Are llama-servers running?", file=sys.stderr)
        return 1

    # Build report
    report = build_report(qwen08b_results, qwen2b_results)
    RESULTS_DIR.mkdir(parents=True, exist_ok=True)
    (RESULTS_DIR / "expanded_benchmark.md").write_text(report, encoding="utf-8")

    # Save JSON
    output = {
        "meta": {
            "sessions": len(dataset["scenarios"]),
            "languages": len(set(sc["language"] for sc in dataset["scenarios"])),
            "scenarios_per_lang": 3,
            "messages_per_session": 6,
            "expected_terms_per_session": 8,
        },
        "stats": {
            "qwen35_0p8b": compute_stats(qwen08b_results),
            "qwen35_2b": compute_stats(qwen2b_results),
        },
        "qwen35_0p8b_live": qwen08b_results,
        "qwen35_2b_live": qwen2b_results,
    }
    (RESULTS_DIR / "expanded_benchmark.json").write_text(
        json.dumps(output, ensure_ascii=False, indent=2), encoding="utf-8")

    print(f"\nWrote {RESULTS_DIR / 'expanded_benchmark.md'}")
    print(f"Wrote {RESULTS_DIR / 'expanded_benchmark.json'}")

    # Print summary
    print("\n" + "=" * 80)
    s08 = output["stats"]["qwen35_0p8b"]
    s2 = output["stats"]["qwen35_2b"]
    print(f"\n{'Metric':<25} {'0.8B':>15} {'2B':>15}")
    print("-" * 55)
    print(f"{'Sessions':<25} {s08['n']:>15} {s2['n']:>15}")
    print(f"{'In-language':<25} {s08['in_language_pct']:>14.1f}% {s2['in_language_pct']:>14.1f}%")
    print(f"{'Usable':<25} {s08['usable_pct']:>14.1f}% {s2['usable_pct']:>14.1f}%")
    print(f"{'Avg coverage':<25} {s08['avg_coverage']*100:>14.1f}% {s2['avg_coverage']*100:>14.1f}%")
    print(f"{'Median coverage':<25} {s08['median_coverage']*100:>14.1f}% {s2['median_coverage']*100:>14.1f}%")
    print(f"{'Avg latency (ms)':<25} {s08['avg_latency_ms']:>15.0f} {s2['avg_latency_ms']:>15.0f}")
    print(f"{'Avg recap chars':<25} {s08['avg_recap_chars']:>15.0f} {s2['avg_recap_chars']:>15.0f}")

    return 0


if __name__ == "__main__":
    sys.exit(main())
