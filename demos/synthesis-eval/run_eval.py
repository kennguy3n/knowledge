#!/usr/bin/env python3
"""
Synthesis-quality evaluation harness — offline, deterministic, CI-runnable.

This is the standing eval the catch-up proposal (A1/G6) calls for. It does NOT
run a model or touch the network: it *scores the already-recorded model output*
checked into the demo result files, so it runs in CI on a box with no GPU and
no llama-server and produces a byte-stable report.

It evaluates two recorded corpora:

  * demos/executive-personas/results/<persona>.json   — the channel recap each
        persona's session produced, scored for term coverage (vs a labeled
        fixture), faithfulness (recap entities grounded in the session
        evidence) and in-language correctness.
  * demos/multilingual-rollup/results/rollup_results.json — the per-language
        synthesis matrix (default on-device 1.7B model) plus the recorded
        1.7B-vs-4B comparison, scored the same way.

Outputs the per-language / per-persona table to
``docs/technical/synthesis-eval.md`` (committed; regenerated here).

Usage:
    python3 run_eval.py            # regenerate docs/technical/synthesis-eval.md
    python3 run_eval.py --check    # regression gate: exit non-zero if any score
                                   #   drops below the documented threshold
    python3 run_eval.py --dump     # print computed metrics as JSON (debug)

The regression gate (``--check``) and the unit tests in
``test_synthesis_eval.py`` are what make this a *gate*, not just a report: CI
fails if coverage or in-language correctness regresses below the floor
documented in ``fixtures/thresholds.json``.
"""

from __future__ import annotations

import argparse
import json
import sys
from dataclasses import dataclass
from pathlib import Path

import scorers
from scorers import Coverage, Grounding, RecapScore

HERE = Path(__file__).resolve().parent
REPO = HERE.parent.parent
PERSONA_DIR = REPO / "demos" / "executive-personas"
ROLLUP_DIR = REPO / "demos" / "multilingual-rollup"
FIXTURES = HERE / "fixtures"
DOC_OUT = REPO / "docs" / "technical" / "synthesis-eval.md"


def _load_json(path: Path) -> dict:
    return json.loads(path.read_text(encoding="utf-8"))


# --------------------------------------------------------------------------- #
# Persona corpus
# --------------------------------------------------------------------------- #
def _persona_dataset_index() -> dict[str, Path]:
    """Map persona id -> dataset path (datasets are numbered, results are not)."""
    out: dict[str, Path] = {}
    for p in sorted((PERSONA_DIR / "dataset").glob("*.json")):
        d = _load_json(p)
        out[d["persona"]["id"]] = p
    return out


def score_personas() -> list[RecapScore]:
    fixture = _load_json(FIXTURES / "expected_terms.json")["personas"]
    datasets = _persona_dataset_index()
    scores: list[RecapScore] = []
    for result_path in sorted((PERSONA_DIR / "results").glob("*.json")):
        result = _load_json(result_path)
        persona = result["persona"]
        pid = persona["id"]
        recap = result.get("artifacts", {}).get("channel_recap", "") or ""
        fx = fixture.get(pid, {})
        language = fx.get("language", persona["languages"][0])
        expected = fx.get("expected_terms", [])

        # Evidence = the session records in the synthesized scope.
        dataset = _load_json(datasets[pid])
        syn_scope = dataset["synthesis"]["scope"]
        evidence = [m["body"] for m in dataset["messages"] if m["scope"] == syn_scope]

        scores.append(RecapScore(
            label=persona["name"],
            language=language,
            script=scorers.script_of(language),
            coverage=scorers.term_coverage(recap, expected),
            grounding=scorers.ungrounded_entities(recap, evidence),
            in_lang=scorers.in_language(language, recap),
            notes=[pid, persona["role"], persona["country"]],
        ))
    return scores


# --------------------------------------------------------------------------- #
# Multilingual matrix corpus
# --------------------------------------------------------------------------- #
def score_multilingual() -> list[RecapScore]:
    results = _load_json(ROLLUP_DIR / "results" / "rollup_results.json")
    dataset = _load_json(ROLLUP_DIR / "dataset" / "multilingual-rollup.json")
    fixture = _load_json(FIXTURES / "expected_terms.json")["multilingual"]
    ev_by_lang = dataset["multilingual_matrix"]["languages"]

    scores: list[RecapScore] = []
    for lang, row in results["multilingual_matrix"].items():
        recap = row.get("recap", "") or ""
        expected = fixture.get(lang, {}).get("expected_terms", [])
        evidence = ev_by_lang.get(lang, [])
        scores.append(RecapScore(
            label=lang,
            language=lang,
            script=scorers.script_of(lang),
            coverage=scorers.term_coverage(recap, expected),
            grounding=scorers.ungrounded_entities(recap, evidence),
            in_lang=scorers.in_language(lang, recap),
            notes=["matrix-1.7B"],
        ))
    return scores


@dataclass
class ModelComparisonRow:
    language: str
    script: str
    in_lang_17b: bool
    in_lang_4b: bool
    usable_17b: bool
    usable_4b: bool


def model_comparison() -> list[ModelComparisonRow]:
    results = _load_json(ROLLUP_DIR / "results" / "rollup_results.json")
    cmp = results.get("model_comparison_1p7b_vs_4b", {})
    rows: list[ModelComparisonRow] = []
    for lang, row in cmp.items():
        m17, m4b = row.get("1.7B", {}), row.get("4B", {})
        rows.append(ModelComparisonRow(
            language=lang,
            script=row.get("script", scorers.script_of(lang)),
            in_lang_17b=scorers.in_language(lang, m17.get("recap", "")),
            in_lang_4b=scorers.in_language(lang, m4b.get("recap", "")),
            usable_17b=bool(m17.get("quality", {}).get("usable")),
            usable_4b=bool(m4b.get("quality", {}).get("usable")),
        ))
    return rows


# --------------------------------------------------------------------------- #
# Regression gate
# --------------------------------------------------------------------------- #
@dataclass
class Failure:
    where: str
    detail: str


def check_gate(personas: list[RecapScore], multilingual: list[RecapScore]) -> list[Failure]:
    """Compare measured scores against the documented thresholds. Returns the
    list of regressions (empty == gate passes)."""
    thresholds = _load_json(FIXTURES / "thresholds.json")
    failures: list[Failure] = []

    def _check_group(group_name: str, scores: list[RecapScore], cfg: dict) -> None:
        min_cov = cfg["min_term_coverage"]
        max_ung = cfg["max_ungrounded_entities"]
        in_lang_baseline: dict[str, bool] = cfg.get("in_language_baseline", {})
        for s in scores:
            cov = s.coverage.fraction if s.coverage else 1.0
            if cov + 1e-9 < min_cov:
                failures.append(Failure(
                    f"{group_name}:{s.label}",
                    f"term coverage {cov:.2f} < threshold {min_cov:.2f} "
                    f"(missing {s.coverage.missing if s.coverage else []})"))
            ung = len(s.grounding.ungrounded) if s.grounding else 0
            if ung > max_ung:
                failures.append(Failure(
                    f"{group_name}:{s.label}",
                    f"{ung} ungrounded entities > threshold {max_ung} "
                    f"({s.grounding.ungrounded if s.grounding else []})"))
            # In-language is a per-label baseline: a recap that is in-language
            # in the baseline must stay in-language. Known-failing rows (the
            # 1.7B model on CJK/Arabic) are baselined False and documented as
            # known limitations — the gate fails only on a *regression* from a
            # passing baseline, never re-asserts a known failure.
            if s.label in in_lang_baseline:
                want = in_lang_baseline[s.label]
                if want and not s.in_lang:
                    failures.append(Failure(
                        f"{group_name}:{s.label}",
                        "in-language regressed: baseline expects in-language, "
                        "recap is not"))

    _check_group("persona", personas, thresholds["personas"])
    _check_group("multilingual", multilingual, thresholds["multilingual"])
    return failures


# --------------------------------------------------------------------------- #
# Report
# --------------------------------------------------------------------------- #
def _yn(b: bool | None) -> str:
    if b is None:
        return "—"
    return "yes" if b else "**no**"


def _pct(c: Coverage | None) -> str:
    if not c:
        return "—"
    return f"{len(c.matched)}/{len(c.expected)} ({c.fraction * 100:.0f}%)"


def _ground(g: Grounding | None) -> str:
    if not g:
        return "—"
    if not g.ungrounded:
        return f"{len(g.entities)}/{len(g.entities)} grounded"
    return f"**{len(g.ungrounded)} ungrounded** ({', '.join(g.ungrounded)})"


def build_report(personas: list[RecapScore], multilingual: list[RecapScore],
                 cmp_rows: list[ModelComparisonRow], gate_ok: bool) -> str:
    thresholds = _load_json(FIXTURES / "thresholds.json")
    lines: list[str] = []
    a = lines.append

    a("# Synthesis-quality evaluation")
    a("")
    a("> **Generated by** `demos/synthesis-eval/run_eval.py` — do not edit by "
      "hand. Re-run `python3 demos/synthesis-eval/run_eval.py` after the recorded "
      "demo outputs change.")
    a("")
    a("This is the standing, **offline** synthesis-quality eval. It scores the "
      "*already-recorded* model output checked into the demo result files; it "
      "runs no model and makes no network call, so it is deterministic and "
      "CI-runnable on a box with no GPU. It closes gap **G6** (\"no "
      "retrieval/synthesis quality eval harness\") from the catch-up proposal and "
      "grades the **G1** synthesis-quality concern with numbers instead of "
      "anecdote.")
    a("")
    a("## What is measured")
    a("")
    a("| Scorer | Question | How |")
    a("|--------|----------|-----|")
    a("| **Term coverage** | Does the recap surface the session's key facts? | "
      "Fraction of a *labeled* expected-terms fixture (`fixtures/expected_terms.json`) "
      "the recap mentions (case-insensitive substring). |")
    a("| **Faithfulness / grounding** | Does the recap invent entities? | Recap "
      "entities (identifiers, codes, brand names, proper nouns) that do **not** "
      "appear in the session evidence are flagged as likely hallucinations. |")
    a("| **In-language** | Is the recap in the session's own language? | A "
      "Unicode script detector compares the recap's alphabetic characters by "
      "script, tolerating embedded Latin product names (`MySQL`, `SKU-6310`). |")
    a("")
    a("The script detector and salient-term tokeniser match the production crate "
      "(`crates/synthesis_pipeline/src/eval.rs`, exercised by `cargo test`), so "
      "the demo, the CI gate and the shipped library agree on what they measure.")
    a("")

    # --- Multilingual matrix ------------------------------------------------ #
    a("## 1. Multilingual matrix — default on-device model (Bonsai-1.7B Q2_0)")
    a("")
    a("Same business situation expressed natively per language; the recap must "
      "come back **in-language**. This is where the default 2-bit model is "
      "weakest — the non-Latin scripts (CJK, spaceless Thai, RTL Arabic) are the "
      "stress cases.")
    a("")
    a("| Language | Script | Term coverage | Faithfulness | In-language |")
    a("|----------|--------|---------------|--------------|-------------|")
    for s in multilingual:
        a(f"| {s.label} | {s.script} | {_pct(s.coverage)} | "
          f"{_ground(s.grounding)} | {_yn(s.in_lang)} |")
    a("")
    ml_inlang = sum(1 for s in multilingual if s.in_lang)
    a(f"_In-language: **{ml_inlang}/{len(multilingual)}** languages. "
      "The misses are the documented 2-bit non-Latin limitation (G1): the model "
      "drops to a placeholder or answers in English on the hardest scripts._")
    a("")

    # --- 1.7B vs 4B --------------------------------------------------------- #
    if cmp_rows:
        a("## 2. Model upgrade probe — 1.7B vs opt-in 4B (in-language)")
        a("")
        a("The recorded `--compare-4b` probe replays each language's synthesis "
          "prompt against both models. This quantifies the in-language win from "
          "the opt-in 4B upgrade the proposal recommends as the default for "
          "non-Latin deployments.")
        a("")
        a("| Language | Script | 1.7B in-language | 4B in-language | 1.7B usable | 4B usable |")
        a("|----------|--------|------------------|----------------|-------------|-----------|")
        for r in cmp_rows:
            a(f"| {r.language} | {r.script} | {_yn(r.in_lang_17b)} | "
              f"{_yn(r.in_lang_4b)} | {_yn(r.usable_17b)} | {_yn(r.usable_4b)} |")
        a("")
        won = [r.language for r in cmp_rows if r.in_lang_4b and not r.in_lang_17b]
        if won:
            a(f"_4B recovers in-language synthesis on **{', '.join(won)}** where "
              "1.7B fails — the evidence behind defaulting non-Latin deployments "
              "to the 4B tier._")
        a("")

    # --- Personas ----------------------------------------------------------- #
    a("## 3. Executive personas — channel recap quality")
    a("")
    a("Each persona is a realistic SME executive session; the recap is the "
      "briefing the executive actually reads. Coverage here is against the "
      "labeled `synthesis.expect_terms_any` fixture.")
    a("")
    a("| Persona | Language | Term coverage | Faithfulness | In-language |")
    a("|---------|----------|---------------|--------------|-------------|")
    for s in personas:
        a(f"| {s.label} ({s.notes[2]}) | {s.language} | {_pct(s.coverage)} | "
          f"{_ground(s.grounding)} | {_yn(s.in_lang)} |")
    a("")

    # --- Thresholds / gate -------------------------------------------------- #
    a("## 4. Regression gate")
    a("")
    a("`python3 demos/synthesis-eval/run_eval.py --check` (and the `unittest` "
      "suite in `demos/synthesis-eval/test_synthesis_eval.py`) fail CI if any "
      "score drops below the documented floor. The floor is the **current "
      "measured baseline** — the gate locks in today's honest numbers and fails "
      "on a regression, in keeping with the team's audit-before-artifacts ethos.")
    a("")
    a("| Group | Min term coverage | Max ungrounded entities | In-language baseline |")
    a("|-------|-------------------|-------------------------|----------------------|")
    for grp in ("personas", "multilingual"):
        cfg = thresholds[grp]
        baseline = cfg.get("in_language_baseline", {})
        in_pass = sum(1 for v in baseline.values() if v)
        a(f"| {grp} | {cfg['min_term_coverage']:.2f} | "
          f"{cfg['max_ungrounded_entities']} | {in_pass}/{len(baseline)} expected in-language |")
    a("")
    a(f"_Current status: gate **{'passes' if gate_ok else 'FAILS'}**._")
    a("")
    return "\n".join(lines)


# --------------------------------------------------------------------------- #
# main
# --------------------------------------------------------------------------- #
def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--check", action="store_true",
                    help="regression gate: exit non-zero if a score regresses")
    ap.add_argument("--dump", action="store_true",
                    help="print computed metrics as JSON and exit")
    args = ap.parse_args(argv)

    personas = score_personas()
    multilingual = score_multilingual()
    cmp_rows = model_comparison()

    if args.dump:
        def _row(s: RecapScore) -> dict:
            return {
                "label": s.label, "language": s.language, "script": s.script,
                "coverage": {"matched": len(s.coverage.matched),
                             "expected": len(s.coverage.expected),
                             "fraction": round(s.coverage.fraction, 3),
                             "missing": s.coverage.missing},
                "ungrounded": s.grounding.ungrounded,
                "entities": s.grounding.entities,
                "in_language": s.in_lang,
            }
        print(json.dumps({
            "personas": [_row(s) for s in personas],
            "multilingual": [_row(s) for s in multilingual],
            "model_comparison": [vars(r) for r in cmp_rows],
        }, ensure_ascii=False, indent=2))
        return 0

    failures = check_gate(personas, multilingual)
    gate_ok = not failures

    if args.check:
        if failures:
            print("SYNTHESIS-EVAL REGRESSION GATE: FAIL", file=sys.stderr)
            for f in failures:
                print(f"  - {f.where}: {f.detail}", file=sys.stderr)
            return 1
        print("SYNTHESIS-EVAL REGRESSION GATE: PASS "
              f"({len(personas)} personas, {len(multilingual)} languages)")
        return 0

    report = build_report(personas, multilingual, cmp_rows, gate_ok)
    DOC_OUT.write_text(report, encoding="utf-8")
    print(f"wrote {DOC_OUT.relative_to(REPO)} "
          f"({len(personas)} personas, {len(multilingual)} languages, "
          f"gate {'passes' if gate_ok else 'FAILS'})")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
