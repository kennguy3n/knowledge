#!/usr/bin/env python3
"""
Public multilingual synthesis leaderboard — offline, deterministic, reproducible.

This is the published, reproducible synthesis-quality leaderboard: a board that
mirrors how Mem0/Zep publish eval numbers, but adds Knowledge's privacy +
multilingual axes, where it can win outright (SEA/GCC languages, on-device, PQC).
It builds *on top of* the synthesis-eval harness
(`scorers.py` + `run_eval.py`): it re-uses the same three deterministic scorers
and the same already-recorded model output, then **aggregates them per
language** into a single board.

It runs no model and makes no network call — it only re-scores JSON checked into
the repo — so a single command regenerates `docs/technical/multilingual-leaderboard.md`
byte-for-byte on a box with no GPU. That reproducibility *is* the point: a buyer
can re-run the board themselves and get the committed numbers.

What it produces
----------------
1. **Per-language board** (default on-device Bonsai-1.7B Q2_0): the three
   scorers — term coverage, faithfulness/grounding, in-language — aggregated
   across *every recorded recap for that language* (the multilingual matrix
   recap plus any executive-persona recap in the same language).
2. **Model-tier comparison** (1.7B vs opt-in 4B) for the languages where the
   `--compare-4b` probe was recorded.
3. **Pending languages**: SEA/GCC languages the project README claims that have
   a labeled dataset + expected-terms fixture but **no recorded model output
   yet** — listed honestly as `pending`, never with fabricated scores.

Usage
-----
    python3 demos/synthesis-eval/leaderboard.py            # regenerate the doc + snapshot
    python3 demos/synthesis-eval/leaderboard.py --check     # determinism/regression gate
    python3 demos/synthesis-eval/leaderboard.py --dump      # print computed metrics as JSON

The `--check` gate fails if the committed doc or snapshot is not byte-identical
to a fresh regeneration — i.e. if someone edited the generated artifacts by hand
or the recorded inputs changed without refreshing the board.
"""

from __future__ import annotations

import argparse
import json
import sys
from dataclasses import dataclass, field
from pathlib import Path

import run_eval
import scorers
from scorers import RecapScore

HERE = Path(__file__).resolve().parent
REPO = HERE.parent.parent
ROLLUP_DIR = REPO / "demos" / "multilingual-rollup"
FIXTURES = HERE / "fixtures"
DOC_OUT = REPO / "docs" / "technical" / "multilingual-leaderboard.md"
SNAPSHOT_OUT = HERE / "leaderboard_snapshot.json"

DEFAULT_MODEL = "Bonsai-1.7B Q2_0"


def _load_json(path: Path) -> dict:
    return json.loads(path.read_text(encoding="utf-8"))


# --------------------------------------------------------------------------- #
# Per-language aggregation
# --------------------------------------------------------------------------- #
@dataclass
class LanguageRow:
    """Per-language aggregate of the three scorers over every recorded recap
    in that language (rollup matrix + same-language persona recaps)."""

    language: str
    script: str
    sources: list[str] = field(default_factory=list)
    matched_terms: int = 0
    expected_terms: int = 0
    entities: int = 0
    ungrounded: int = 0
    in_language_pass: int = 0
    recaps: int = 0

    def add(self, score: RecapScore, source: str) -> None:
        self.sources.append(source)
        self.recaps += 1
        if score.coverage is not None:
            self.matched_terms += len(score.coverage.matched)
            self.expected_terms += len(score.coverage.expected)
        if score.grounding is not None:
            self.entities += len(score.grounding.entities)
            self.ungrounded += len(score.grounding.ungrounded)
        if score.in_lang:
            self.in_language_pass += 1

    @property
    def coverage_fraction(self) -> float:
        if self.expected_terms == 0:
            return 1.0
        return self.matched_terms / self.expected_terms

    @property
    def grounded_fraction(self) -> float:
        if self.entities == 0:
            return 1.0
        return 1.0 - self.ungrounded / self.entities

    @property
    def fully_in_language(self) -> bool:
        return self.recaps > 0 and self.in_language_pass == self.recaps


@dataclass
class PendingRow:
    """A language the README claims but for which no model output is recorded
    yet — listed honestly, never scored."""

    language: str
    script: str
    expected_terms: list[str]


def claimed_languages() -> list[str]:
    """Languages with a recorded *or* labeled dataset session, in the dataset's
    own (deterministic) file order. This is the full set the board reasons
    about; the subset with recorded output is scored, the remainder is pending.
    """
    dataset = _load_json(ROLLUP_DIR / "dataset" / "multilingual-rollup.json")
    return list(dataset["multilingual_matrix"]["languages"].keys())


def aggregate() -> tuple[list[LanguageRow], list[PendingRow]]:
    """Aggregate the recorded persona + rollup recaps per language, and surface
    the README-claimed languages that have no recorded recap as pending."""
    personas = run_eval.score_personas()
    multilingual = run_eval.score_multilingual()
    fixture_ml = _load_json(FIXTURES / "expected_terms.json")["multilingual"]

    rows: dict[str, LanguageRow] = {}

    def _row(language: str) -> LanguageRow:
        if language not in rows:
            rows[language] = LanguageRow(language=language,
                                         script=scorers.script_of(language))
        return rows[language]

    # The multilingual matrix contributes one recorded recap per language.
    for s in multilingual:
        _row(s.language).add(s, "rollup-matrix")
    # Each executive persona contributes its channel recap to its language.
    for s in personas:
        _row(s.language).add(s, f"persona:{s.label}")

    # Order the scored board by the dataset's canonical language order, then any
    # extra (e.g. persona-only) languages alphabetically for stability.
    canonical = claimed_languages()
    order = {lang: i for i, lang in enumerate(canonical)}
    scored = sorted(rows.values(),
                    key=lambda r: (order.get(r.language, len(order)), r.language))

    recorded = set(rows)
    pending = [
        PendingRow(language=lang, script=scorers.script_of(lang),
                   expected_terms=fixture_ml.get(lang, {}).get("expected_terms", []))
        for lang in canonical if lang not in recorded
    ]
    return scored, pending


# --------------------------------------------------------------------------- #
# Snapshot (structured regression artifact)
# --------------------------------------------------------------------------- #
def build_snapshot(scored: list[LanguageRow], pending: list[PendingRow],
                   cmp_rows: list[run_eval.ModelComparisonRow]) -> dict:
    """The committed, machine-readable record the `--check` gate diffs against.
    Numbers are rounded to a fixed precision so the artifact is byte-stable."""
    return {
        "_README": (
            "Committed snapshot of the multilingual synthesis leaderboard. "
            "Regenerate with `python3 demos/synthesis-eval/leaderboard.py`; the "
            "`--check` gate fails if this file or docs/technical/"
            "multilingual-leaderboard.md drifts from a fresh regeneration. Do "
            "not edit by hand."),
        "default_model": DEFAULT_MODEL,
        "per_language": [
            {
                "language": r.language,
                "script": r.script,
                "recaps": r.recaps,
                "sources": r.sources,
                "term_coverage": {
                    "matched": r.matched_terms,
                    "expected": r.expected_terms,
                    "fraction": round(r.coverage_fraction, 4),
                },
                "faithfulness": {
                    "entities": r.entities,
                    "ungrounded": r.ungrounded,
                    "grounded_fraction": round(r.grounded_fraction, 4),
                },
                "in_language": {
                    "passed": r.in_language_pass,
                    "recaps": r.recaps,
                    "fully_in_language": r.fully_in_language,
                },
            }
            for r in scored
        ],
        "model_tier_comparison": [
            {
                "language": r.language,
                "script": r.script,
                "in_language_1p7b": r.in_lang_17b,
                "in_language_4b": r.in_lang_4b,
                "usable_1p7b": r.usable_17b,
                "usable_4b": r.usable_4b,
            }
            for r in cmp_rows
        ],
        "pending": [
            {"language": p.language, "script": p.script,
             "expected_terms": p.expected_terms}
            for p in pending
        ],
    }


# --------------------------------------------------------------------------- #
# Report
# --------------------------------------------------------------------------- #
def _yn(b: bool | None) -> str:
    if b is None:
        return "—"
    return "yes" if b else "**no**"


def _cov(r: LanguageRow) -> str:
    return f"{r.matched_terms}/{r.expected_terms} ({r.coverage_fraction * 100:.0f}%)"


def _ground(r: LanguageRow) -> str:
    if r.entities == 0:
        return "— (no entities)"
    if r.ungrounded == 0:
        return f"{r.entities}/{r.entities} grounded"
    grounded = r.entities - r.ungrounded
    return f"**{grounded}/{r.entities}** ({r.ungrounded} ungrounded)"


def _inlang(r: LanguageRow) -> str:
    mark = "yes" if r.fully_in_language else "**no**"
    return f"{mark} ({r.in_language_pass}/{r.recaps})"


def build_report(scored: list[LanguageRow], pending: list[PendingRow],
                 cmp_rows: list[run_eval.ModelComparisonRow]) -> str:
    lines: list[str] = []
    a = lines.append

    a("# Multilingual synthesis leaderboard")
    a("")
    a("> **Generated by** `demos/synthesis-eval/leaderboard.py` — do not edit by "
      "hand. Re-run `python3 demos/synthesis-eval/leaderboard.py` after the "
      "recorded demo outputs or fixtures change; "
      "`python3 demos/synthesis-eval/leaderboard.py --check` gates it in CI.")
    a("")
    a("A **public, reproducible** synthesis-quality board, in the spirit of the "
      "eval leaderboards Mem0/Zep publish — but scored on the axes where an "
      "on-device, privacy-first substrate can win outright: **multilingual "
      "breadth (SEA/GCC/CJK/Arabic)** and **in-language** correctness. It is "
      "built on the synthesis-eval harness (`scorers.py`), re-using the same three "
      "deterministic scorers over the same already-recorded model output, so it "
      "runs offline with no GPU and regenerates byte-for-byte from one command.")
    a("")
    a("It complements the "
      "per-persona / per-matrix view in "
      "[`synthesis-eval.md`](synthesis-eval.md): that doc scores each recap; "
      "this one **rolls the scores up per language** and adds a model-tier "
      "comparison and an honest pending list.")
    a("")

    # --- Methodology -------------------------------------------------------- #
    a("## Methodology")
    a("")
    a("| Scorer | Question | How |")
    a("|--------|----------|-----|")
    a("| **Term coverage** | Does the recap surface the session's key facts? | "
      "Micro-averaged fraction of the *labeled* expected-terms fixtures "
      "(`fixtures/expected_terms.json`) the recaps mention — "
      "`Σ matched / Σ expected` across every recorded recap in the language "
      "(case-insensitive substring). |")
    a("| **Faithfulness / grounding** | Does the recap invent entities? | "
      "Recap entities (identifiers, codes, brand names, proper nouns) absent "
      "from the session evidence are flagged as likely hallucinations; summed "
      "across the language's recaps. |")
    a("| **In-language** | Is the recap in the session's own language? | A "
      "Unicode script detector compares the recap's alphabetic characters by "
      "script, tolerating embedded Latin product names (`MySQL`, `SKU-6310`). "
      "`fully in-language` requires *every* recorded recap for the language to "
      "pass. |")
    a("")
    a("Sources aggregated per language: the multilingual matrix recap "
      "(`demos/multilingual-rollup/results/rollup_results.json`) and any "
      "executive-persona channel recap in the same language "
      "(`demos/executive-personas/results/*.json`). The scorers and the script "
      "detector match the production crate "
      "(`crates/synthesis_pipeline/src/eval.rs`, exercised by `cargo test`), so "
      "the board, the CI gate and the shipped library agree on what they "
      "measure.")
    a("")
    a("**Honesty contract:** every number below is computed from a recap that "
      "was *actually recorded* from the named model. Languages with no recorded "
      "run are listed as `pending` (§3) — never scored with a placeholder.")
    a("")

    # --- 1. Per-language board --------------------------------------------- #
    a(f"## 1. Per-language board — default on-device model ({DEFAULT_MODEL})")
    a("")
    a("Aggregated across the recorded recaps for each language. The non-Latin "
      "scripts (CJK, spaceless Thai, RTL Arabic) are the stress cases for the "
      "default 2-bit model.")
    a("")
    a("| Language | Script | Recaps | Term coverage | Faithfulness | In-language |")
    a("|----------|--------|--------|---------------|--------------|-------------|")
    for r in scored:
        a(f"| {r.language} | {r.script} | {r.recaps} | {_cov(r)} | "
          f"{_ground(r)} | {_inlang(r)} |")
    a("")
    fully = sum(1 for r in scored if r.fully_in_language)
    a(f"_In-language: **{fully}/{len(scored)}** recorded languages are fully "
      "in-language on the default model. The misses are the documented 2-bit "
      "non-Latin limitation — the model drops to a placeholder or answers "
      "in English on the hardest scripts; see §2 for the 4B recovery._")
    a("")

    # --- 2. Model-tier comparison ------------------------------------------ #
    if cmp_rows:
        a("## 2. Model-tier comparison — 1.7B vs opt-in 4B")
        a("")
        a("The recorded `--compare-4b` probe replays each language's synthesis "
          "prompt against both the default on-device 1.7B and the opt-in 4B. "
          "This is the evidence behind defaulting non-Latin deployments to the "
          "4B tier.")
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
              "1.7B fails._")
        a("")

    # --- 3. Pending --------------------------------------------------------- #
    a("## 3. Pending languages (claimed, not yet recorded)")
    a("")
    if pending:
        a("These SEA/GCC languages the project README claims have a labeled "
          "dataset session and expected-terms fixture, but **no recorded model "
          "output yet**. They are listed honestly as `pending`: a live "
          "`python3 demos/multilingual-rollup/run_rollup.py [--compare-4b]` run "
          "records their recaps, after which re-running the generator scores "
          "them automatically.")
        a("")
        a("| Language | Script | Status | Labeled expected terms |")
        a("|----------|--------|--------|------------------------|")
        for p in pending:
            terms = ", ".join(f"`{t}`" for t in p.expected_terms) or "—"
            a(f"| {p.language} | {p.script} | pending | {terms} |")
        a("")
    else:
        a("_None: every README-claimed language in the dataset has a recorded "
          "recap._")
        a("")

    # --- 4. Reproduce ------------------------------------------------------- #
    a("## 4. Reproduce")
    a("")
    a("The board regenerates deterministically from one command — no model, no "
      "network, no GPU:")
    a("")
    a("```bash")
    a("python3 demos/synthesis-eval/leaderboard.py          # regenerate this doc + snapshot")
    a("python3 demos/synthesis-eval/leaderboard.py --check   # CI gate: fail on any drift")
    a("```")
    a("")
    a("The committed snapshot `demos/synthesis-eval/leaderboard_snapshot.json` "
      "is the machine-readable record the gate diffs against. To refresh the "
      "whole synthesis-eval surface (this board + `synthesis-eval.md`) and run "
      "every check, use `demos/synthesis-eval/refresh.sh`.")
    a("")
    return "\n".join(lines)


# --------------------------------------------------------------------------- #
# main
# --------------------------------------------------------------------------- #
def _render() -> tuple[str, str, list[LanguageRow], list[PendingRow]]:
    """Compute everything once and return (markdown, snapshot_json, scored,
    pending) — pure, so the writer, the `--check` gate and the tests share one
    code path and the scoring pipeline runs a single time per render."""
    scored, pending = aggregate()
    cmp_rows = run_eval.model_comparison()
    report = build_report(scored, pending, cmp_rows)
    snapshot = build_snapshot(scored, pending, cmp_rows)
    snapshot_json = json.dumps(snapshot, ensure_ascii=False, indent=2) + "\n"
    return report, snapshot_json, scored, pending


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--check", action="store_true",
                    help="determinism/regression gate: exit non-zero if the "
                         "committed doc or snapshot drifts from a regeneration")
    ap.add_argument("--dump", action="store_true",
                    help="print computed metrics as JSON and exit")
    args = ap.parse_args(argv)

    report, snapshot_json, scored, pending = _render()

    if args.dump:
        sys.stdout.write(snapshot_json)
        return 0

    if args.check:
        problems: list[str] = []
        if not DOC_OUT.exists():
            problems.append(f"{DOC_OUT.relative_to(REPO)} is missing")
        elif DOC_OUT.read_text(encoding="utf-8") != report:
            problems.append(f"{DOC_OUT.relative_to(REPO)} is stale "
                            "(differs from a fresh regeneration)")
        if not SNAPSHOT_OUT.exists():
            problems.append(f"{SNAPSHOT_OUT.relative_to(REPO)} is missing")
        elif SNAPSHOT_OUT.read_text(encoding="utf-8") != snapshot_json:
            problems.append(f"{SNAPSHOT_OUT.relative_to(REPO)} is stale "
                            "(differs from a fresh regeneration)")
        if problems:
            print("MULTILINGUAL-LEADERBOARD GATE: FAIL", file=sys.stderr)
            for p in problems:
                print(f"  - {p}", file=sys.stderr)
            print("  run `python3 demos/synthesis-eval/leaderboard.py` to refresh",
                  file=sys.stderr)
            return 1
        print("MULTILINGUAL-LEADERBOARD GATE: PASS (doc + snapshot reproduce "
              "byte-for-byte)")
        return 0

    DOC_OUT.write_text(report, encoding="utf-8")
    SNAPSHOT_OUT.write_text(snapshot_json, encoding="utf-8")
    print(f"wrote {DOC_OUT.relative_to(REPO)} and "
          f"{SNAPSHOT_OUT.relative_to(REPO)} "
          f"({len(scored)} recorded languages, {len(pending)} pending)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
