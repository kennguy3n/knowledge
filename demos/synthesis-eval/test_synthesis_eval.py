#!/usr/bin/env python3
"""
Unit + regression tests for the synthesis-quality eval harness.

Run with the stdlib test runner (no third-party deps):

    python3 -m unittest discover -s demos/synthesis-eval -p 'test_*.py'

or directly:

    python3 demos/synthesis-eval/test_synthesis_eval.py

The class `RegressionGate` is the CI gate proper: it loads the real recorded
demo outputs and asserts every score is at or above the documented floor in
`fixtures/thresholds.json`. The other classes pin the scorer semantics so a
future refactor cannot silently change what "coverage", "grounded" or
"in-language" mean.
"""

from __future__ import annotations

import unittest

import run_eval
import scorers


class InLanguage(unittest.TestCase):
    def test_latin_passes(self):
        self.assertTrue(scorers.in_language("French", "Le litige CartoNord est résolu."))

    def test_latin_with_cjk_fails(self):
        # A French recap with stray CJK is not in-language.
        self.assertFalse(scorers.in_language("French", "Le litige 決定 est résolu."))

    def test_cjk_passes_with_embedded_latin(self):
        # Embedded Latin product names are tolerated inside a CJK recap.
        self.assertTrue(scorers.in_language(
            "Japanese", "AX-7サーボの過熱はファームウェアが原因である。"))

    def test_arabic_session_answered_in_english_fails(self):
        self.assertFalse(scorers.in_language(
            "Arabic", "The billing database migration from MySQL to Postgres."))

    def test_arabic_passes(self):
        self.assertTrue(scorers.in_language(
            "Arabic", "ترحيل قاعدة بيانات الفوترة من MySQL إلى Postgres."))

    def test_empty_recap_never_in_language(self):
        self.assertFalse(scorers.in_language("English", ""))
        self.assertFalse(scorers.in_language("Japanese", "…"))


class TermCoverage(unittest.TestCase):
    def test_case_insensitive_substring(self):
        cov = scorers.term_coverage("The CARTONORD dispute over the avoir.",
                                    ["cartonord", "avoir", "humidité"])
        self.assertEqual(cov.matched, ["cartonord", "avoir"])
        self.assertEqual(cov.missing, ["humidité"])
        self.assertAlmostEqual(cov.fraction, 2 / 3)

    def test_empty_expected_is_full_coverage(self):
        self.assertEqual(scorers.term_coverage("anything", []).fraction, 1.0)

    def test_order_preserved(self):
        cov = scorers.term_coverage("b a", ["a", "b", "c"])
        self.assertEqual(cov.matched, ["a", "b"])


class Entities(unittest.TestCase):
    def test_identifiers(self):
        ents = scorers.recap_entities("Lot BR-2505 invoice FA-2025-0411 SKU-8842 v2.4.1")
        for e in ("BR-2505", "FA-2025-0411", "SKU-8842", "v2.4.1"):
            self.assertIn(e, ents)

    def test_camelcase_and_acronym(self):
        ents = scorers.recap_entities("migrate to MySQL via CartoNord per EUR rules")
        self.assertIn("MySQL", ents)
        self.assertIn("CartoNord", ents)
        self.assertIn("EUR", ents)

    def test_ordinal_excluded(self):
        self.assertNotIn("6th", scorers.recap_entities("delivered on May 6th"))

    def test_two_letter_acronym_excluded(self):
        self.assertNotIn("VP", scorers.recap_entities("the VP approved it"))

    def test_sentence_initial_proper_noun_excluded(self):
        # "Decision" opens the 2nd sentence -> function word, not an entity.
        ents = scorers.recap_entities("Priya leads. Decision was made by Adyen.")
        self.assertNotIn("Decision", ents)
        self.assertIn("Adyen", ents)
        self.assertNotIn("Priya", ents)  # sentence-initial -> skipped (precision-first)


class Grounding(unittest.TestCase):
    def test_grounded_entity(self):
        g = scorers.ungrounded_entities("Dispute over CartoNord lot BR-2505.",
                                        ["CartoNord livre le lot BR-2505."])
        self.assertEqual(g.ungrounded, [])
        self.assertEqual(g.grounded_fraction, 1.0)

    def test_digit_separator_tolerance(self):
        # Recap "12,600" is grounded by evidence "12 600".
        g = scorers.ungrounded_entities("Credit of 12,600 EUR.",
                                        ["avoir de 12 600 EUR"])
        self.assertEqual(g.ungrounded, [])

    def test_hallucinated_entity_flagged(self):
        g = scorers.ungrounded_entities("Migrate to Oracle next sprint.",
                                        ["Migrate the billing database to Postgres."])
        self.assertIn("Oracle", g.ungrounded)

    def test_no_entities_is_vacuously_grounded(self):
        g = scorers.ungrounded_entities("the team agreed to move on", ["evidence"])
        self.assertEqual(g.entities, [])
        self.assertEqual(g.ungrounded, [])


class HarnessWiring(unittest.TestCase):
    def test_personas_load(self):
        scores = run_eval.score_personas()
        self.assertEqual(len(scores), 5)
        names = {s.label for s in scores}
        self.assertIn("Élise Moreau", names)

    def test_multilingual_load(self):
        scores = run_eval.score_multilingual()
        self.assertEqual(len(scores), 10)
        langs = {s.label for s in scores}
        self.assertEqual(langs, set(scorers.SCRIPTS) - {"Hindi", "Portuguese"})

    def test_report_builds(self):
        personas = run_eval.score_personas()
        multilingual = run_eval.score_multilingual()
        cmp_rows = run_eval.model_comparison()
        report = run_eval.build_report(personas, multilingual, cmp_rows, True)
        self.assertIn("# Synthesis-quality evaluation", report)
        self.assertIn("| Language | Script | Term coverage", report)


class RegressionGate(unittest.TestCase):
    """The CI gate: real recorded outputs must meet the documented floor."""

    def test_baseline_passes(self):
        personas = run_eval.score_personas()
        multilingual = run_eval.score_multilingual()
        failures = run_eval.check_gate(personas, multilingual)
        self.assertEqual(failures, [], f"unexpected regressions: {failures}")

    def test_baseline_labels_match_scored_labels(self):
        # Guard against a typo'd in_language_baseline key silently disabling the
        # in-language assertion for a label that does not exist.
        import json
        thresholds = json.loads((run_eval.FIXTURES / "thresholds.json").read_text(
            encoding="utf-8"))
        for group, scores in (("personas", run_eval.score_personas()),
                              ("multilingual", run_eval.score_multilingual())):
            labels = {s.label for s in scores}
            for key in thresholds[group].get("in_language_baseline", {}):
                self.assertIn(key, labels,
                              f"{group} baseline key {key!r} matches no scored label")

    def test_gate_catches_coverage_regression(self):
        # Synthesise a below-floor recap and confirm the gate trips.
        bad = scorers.RecapScore(
            label="English", language="English", script="Latin",
            coverage=scorers.term_coverage("nothing relevant here",
                                            ["Postgres", "MySQL", "Priya", "SKU-8842"]),
            grounding=scorers.ungrounded_entities("nothing relevant here", ["evidence"]),
            in_lang=True)
        failures = run_eval.check_gate([], [bad])
        self.assertTrue(any("term coverage" in f.detail for f in failures))

    def test_gate_catches_in_language_regression(self):
        # A French recap that comes back in CJK must trip the in-language gate.
        bad = scorers.RecapScore(
            label="French", language="French", script="Latin",
            coverage=scorers.term_coverage("CartoNord FA-2025-0411 12 600 BR-2505 humidité",
                                           ["CartoNord", "FA-2025-0411", "12 600",
                                            "BR-2505", "humidité"]),
            grounding=scorers.Grounding(entities=[], ungrounded=[]),
            in_lang=False)
        failures = run_eval.check_gate([], [bad])
        self.assertTrue(any("in-language regressed" in f.detail for f in failures))


if __name__ == "__main__":
    unittest.main(verbosity=2)
